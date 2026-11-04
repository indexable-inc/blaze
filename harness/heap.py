#!/usr/bin/env python3
"""heap.py FOLDED [ROWS] [--syntax]: attributes a heap profile to salsa queries and to blaze's own structures.

FOLDED is what `jeprof --collapsed` makes of a jemalloc heap profile (on macOS, what
`heapfold.py` makes of it). blaze no longer writes one: its `heap-profile` feature went on
2026-11-03, as nothing had made jemalloc its allocator since v2 (mimalloc is, `heaps.rs`), so the
profile it dumped held none of blaze's allocations. Profiles written before then still read.

Each allocation goes to the innermost salsa ingredient on its stack: a query's `value` (what the
memo keeps, which an `lru` can let go of), its `record` (the dependencies and revisions salsa keeps
of the memo, which stay when the value goes), an `interned` value, or a tracked struct or input.
What no ingredient made goes to blaze's own structures by the function that allocated it.

`--syntax` also says what of it is syntax: syntax-tree's green nodes (the trees, which a memo of
`parse` or anything holding a `GreenNode` keeps), its flat trees (the same, as arrays), its
red nodes (cursors over a tree), the
lexer's and parser's buffers, ast id maps and span maps, each by the ingredient that made it.
"""
import collections, re, sys

INGREDIENT = re.compile(r"salsa::(function|interned|input|tracked_struct)::IngredientImpl<(.+?)>>::\w+")
EXECUTE = re.compile(r"<(.+?) as salsa::function::Configuration>::execute")
RECORD = ("pop_into_revisions", "OriginAndExtra", "LazyMemoEntries", "insert_memo", "MemoTable",
          "QueryRevisions", "add_read", "report_tracked_read")
OWN = [("symbol table", "SymbolTable"), ("vfs / file loading", "vfs::"), ("vfs / file loading", "load_cargo"),
       ("project model", "project_model"), ("splice", "blaze::splice"), ("checkouts", "blaze::Checkouts"),
       ("object emission", "cranelift_object"), ("object emission", "object::write"),
       ("codegen driver (outside queries)", "hir_ty_codegen::"), ("blaze main (other)", "blaze::")]
# What of an allocation is syntax, by the innermost frame that says so.
SYNTAX = [("flat trees", ("syntax_tree::flat", "FlatTree")),
          ("green nodes", ("syntax_tree::green", "syntax_tree::arc", "GreenNodeBuilder", "SyntaxTreeBuilder")),
          ("red nodes", ("syntax_tree::cursor", "syntax_tree::api")),
          ("lexer and parser buffers", ("parser::", "LexedStr", "rustc_lexer")),
          ("ast id maps", ("AstIdMap",)),
          ("span maps", ("RealSpanMap", "span::map::", "SpanMap")),
          ("token trees", ("tt::", "syntax_bridge::"))]


def short(name):
    name = re.sub(r"::\{closure#\d+\}", "", name)
    name = name.replace("::_::", "::")
    name = re.sub(r"_?Configuration_?$", "", name)
    return re.sub(r"::\w+_shim$", "", name)


def bucket(frames):
    record = False
    for frame in reversed(frames):
        record |= any(it in frame for it in RECORD)
        if m := INGREDIENT.search(frame):
            kind = {"function": "record" if record else "value"}.get(m[1], m[1])
            return kind, short(m[2])
        if m := EXECUTE.search(frame):
            return "record" if record else "value", short(m[1])
    for frame in reversed(frames):
        for label, needle in OWN:
            if needle in frame:
                return "own", label
    return "other", next((f for f in frames[::-1] if "::" in f and "alloc" not in f and "prof_" not in f),
                         frames[-1])[:90]


def syntax(frames):
    """What kind of syntax the allocation is, or None when it is not syntax."""
    for frame in reversed(frames):
        if INGREDIENT.search(frame) or EXECUTE.search(frame):
            return None
        for label, needles in SYNTAX:
            if any(it in frame for it in needles):
                return label
    return None


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    totals, of_syntax = collections.Counter(), collections.Counter()
    for line in open(args[0]):
        stack, _, size = line.rstrip().rpartition(" ")
        frames = stack.split(";")
        where = bucket(frames)
        totals[where] += int(size)
        if "--syntax" in sys.argv and (kind := syntax(frames)):
            of_syntax[(kind, where[1])] += int(size)
    whole = sum(totals.values())
    kinds = collections.Counter()
    for (kind, _), size in totals.items():
        kinds[kind] += size
    print(f"total sampled live: {whole / 2**20:.0f} MB")
    for kind, size in kinds.most_common():
        print(f"{kind}: {size / 2**20:.0f} MB")
    top = int(args[1]) if len(args) > 1 else 45
    for (kind, name), size in totals.most_common(top):
        print(f"{size / 2**20:8.0f} MB {100 * size / whole:5.1f}%  {kind:8} {name[:140]}")
    if "--syntax" in sys.argv:
        by_kind = collections.Counter()
        for (kind, _), size in of_syntax.items():
            by_kind[kind] += size
        print(f"\nsyntax: {sum(by_kind.values()) / 2**20:.0f} MB, {100 * sum(by_kind.values()) / whole:.1f}% of the live heap")
        for kind, size in by_kind.most_common():
            print(f"{size / 2**20:8.0f} MB {100 * size / whole:5.1f}%  {kind}")
        print("by the ingredient that made it:")
        for (kind, name), size in of_syntax.most_common(top):
            print(f"{size / 2**20:8.0f} MB {100 * size / whole:5.1f}%  {kind:26} {name[:110]}")


if __name__ == "__main__":
    main()
