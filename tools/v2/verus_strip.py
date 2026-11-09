"""tools/v2/verus_strip.py: takes vstd out of `verus! { ... }` (docs/v2/plan/design/verus.md).

usage: uv run --frozen tools/v2/verus_strip.py '{"root": "vendor/verus/vstd"}'
       uv run --frozen tools/v2/verus_strip.py '{"root": "vendor/verus/vstd", "check": true}'

blaze parses Verus's syntax natively in a crate with `#![feature(blaze_verify)]`, so vstd's
items need no macro around them. For each `.rs` file under `root`, every `verus! {` that starts
a line at column 0 loses that line and the `}` that closes it (with its `// verus!` comment);
what is between stays byte for byte. vstd's alias for the macro (`use verus as
verus_skip_verusfmt;`, which only hides a block from verusfmt) is stripped the same way, and its
`use` line goes. A `verus!` inside an item (a `macro_rules!` transcriber
emitting one) is left, and listed, as stripping it changes what the macro expands to.

`check` changes nothing and fails when a file still has a column-0 `verus! {`. The run is
idempotent: a stripped file has none.
"""

import sys
from dataclasses import dataclass
from pathlib import Path

from typed_input import fail, parse

SCRIPT = "tools/v2/verus_strip.py"
OPENERS = ("verus! {", "verus_skip_verusfmt! {")
ALIAS = "use verus as verus_skip_verusfmt;"


@dataclass(frozen=True)
class Input:
    root: str
    check: bool = False


class LexError(Exception):
    """Where the Rust lexing of a file went wrong."""


def _skip_string(text: str, i: int) -> int:
    """The index past the string literal whose opening quote is at `i`."""
    i += 1
    while i < len(text):
        c = text[i]
        if c == "\\":
            i += 2
            continue
        if c == '"':
            return i + 1
        i += 1
    raise LexError(f"a string literal opened at byte {i} is not closed")


def _skip_raw_string(text: str, i: int) -> int | None:
    """The index past the raw string (`r"..."`, `r#"..."#`) starting at `i`, or None if none does."""
    j = i + 1
    hashes = 0
    while j < len(text) and text[j] == "#":
        hashes += 1
        j += 1
    if j >= len(text) or text[j] != '"':
        return None
    end = '"' + "#" * hashes
    close = text.find(end, j + 1)
    if close < 0:
        raise LexError(f"a raw string opened at byte {i} is not closed")
    return close + len(end)


def _skip_char_or_lifetime(text: str, i: int) -> int:
    """The index past the char literal at `i`, or past the `'` of a lifetime or label."""
    if i + 1 < len(text) and text[i + 1] == "\\":
        close = text.find("'", i + 2)
        if close < 0:
            raise LexError(f"a char literal opened at byte {i} is not closed")
        # `'\''`: the escaped quote is not the close.
        if close == i + 3 and text[i + 2] == "'":
            close = text.find("'", i + 4)
        return close + 1
    if i + 2 < len(text) and text[i + 2] == "'":
        return i + 3
    return i + 1


def matching_brace(text: str, open_at: int) -> int:
    """The index of the `}` closing the `{` at `open_at`, lexing comments, strings and chars."""
    depth = 0
    i = open_at
    while i < len(text):
        c = text[i]
        if text.startswith("//", i):
            nl = text.find("\n", i)
            i = len(text) if nl < 0 else nl
            continue
        if text.startswith("/*", i):
            nest = 1
            i += 2
            while nest and i < len(text):
                if text.startswith("/*", i):
                    nest += 1
                    i += 2
                elif text.startswith("*/", i):
                    nest -= 1
                    i += 2
                else:
                    i += 1
            if nest:
                raise LexError(f"a block comment before byte {i} is not closed")
            continue
        prev_ident = i > 0 and (text[i - 1].isalnum() or text[i - 1] == "_")
        if c in "rbc" and not prev_ident:
            # `r"`, `r#"`, `br"`, `cr"`: raw; `b"`, `c"`: escaped; `b'`: a byte.
            r = i + 1 if c != "r" and text.startswith("r", i + 1) else i
            if text.startswith("r", r):
                end = _skip_raw_string(text, r)
                if end is not None:
                    i = end
                    continue
            if c != "r" and text.startswith('"', i + 1):
                i = _skip_string(text, i + 1)
                continue
            if c == "b" and text.startswith("'", i + 1):
                i = _skip_char_or_lifetime(text, i + 1)
                continue
        if c == '"':
            i = _skip_string(text, i)
            continue
        if c == "'":
            i = _skip_char_or_lifetime(text, i)
            continue
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0:
                return i
        i += 1
    raise LexError(f"the `{{` at byte {open_at} is not closed")


def _next_open(text: str) -> tuple[int, str] | None:
    """The first column-0 opener in `text`: its index and which one."""
    found: list[tuple[int, str]] = []
    for opener in OPENERS:
        at = 0 if text.startswith(opener) else text.find("\n" + opener)
        if at >= 0:
            found.append((at if at == 0 and text.startswith(opener) else at + 1, opener))
    return min(found) if found else None


def strip(text: str) -> tuple[str, int]:
    """`text` with each column-0 `verus! {` block unwrapped, and how many there were."""
    count = 0
    lines = text.split("\n")
    kept = [line for line in lines if not line.startswith(ALIAS)]
    if len(kept) != len(lines):
        text = "\n".join(kept)
        count += 1
    while True:
        found = _next_open(text)
        if found is None:
            return text, count
        start, opener = found
        brace = start + len(opener) - 1
        close = matching_brace(text, brace)
        # The opening line goes whole when the rest of it is only a comment; so does the
        # closing `}` with the rest of its line.
        open_end = text.find("\n", start)
        open_end = len(text) if open_end < 0 else open_end + 1
        after = text[brace + 1 : open_end].strip()
        if after and not after.startswith("//"):
            raise LexError(f"`{opener}` at byte {start} has code after it on its line")
        close_end = text.find("\n", close)
        close_end = len(text) if close_end < 0 else close_end + 1
        rest = text[close + 1 : close_end].strip()
        if rest and not rest.startswith("//"):
            raise LexError(f"the `}}` closing `{opener}` at byte {start} has code after it on its line")
        line_start = text.rfind("\n", 0, close) + 1
        if text[line_start:close].strip():
            raise LexError(f"the `}}` closing `{opener}` at byte {start} has code before it on its line")
        text = text[:start] + text[open_end:line_start] + text[close_end:]
        count += 1


def main() -> int:
    if len(sys.argv) != 2:
        fail(SCRIPT, 'takes one JSON value: {"root": DIR, "check": BOOL}')
    args = parse(Input, sys.argv[1], SCRIPT)
    root = Path(args.root)
    if not root.is_dir():
        fail(SCRIPT, f"root `{root}` is not a directory")
    stripped = 0
    left: list[str] = []
    remaining: list[str] = []
    for path in sorted(root.rglob("*.rs")):
        text = path.read_text()
        try:
            new, count = strip(text)
        except LexError as err:
            fail(SCRIPT, f"{path}: {err}")
        if count:
            remaining.append(f"{path}: {count}")
        if count and not args.check:
            path.write_text(new)
            stripped += count
        for n, line in enumerate(new.splitlines(), 1):
            if ("verus!" in line or "verus_skip_verusfmt" in line) and not line.lstrip().startswith("//"):
                left.append(f"{path}:{n}: {line.strip()}")
    for line in left:
        print(f"left inside an item: {line}")
    if args.check:
        if remaining:
            for line in remaining:
                print(f"still wrapped: {line}")
            return 1
        return 0
    print(f"stripped {stripped} `verus!` blocks under {root}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
