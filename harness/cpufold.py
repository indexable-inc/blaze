#!/usr/bin/env python3
"""cpufold.py SCRIPT [--comm NAME] [--dso PATH] [--phases FILE]: what share of a `perf` profile's CPU time parsing and lowering take.

SCRIPT is what `perf script` prints of a `perf record -g` of the daemon (frame pointers:
`RUSTFLAGS=-Cforce-frame-pointers=yes`). Two tables. The first gives, for each kind of work,
the share of the samples with a frame of that kind on their stack: a parse asked for by an
item tree counts in both. The second gives each sample to one kind, that of its innermost
frame that is of a kind: what the time was spent doing itself, which adds up to 100%.
`--comm` keeps the samples of processes whose name starts with NAME (default `blaze`), and
`--dso PATH` those with a frame in a binary whose path has PATH in it, for a profile of the
whole machine (`perf record -a`) that other blaze builds ran beside. `--phases FILE` reads the
phases leanprof.py wrote (a profile recorded with `-k CLOCK_MONOTONIC`) and gives both tables
for each phase, with a third: of the samples revalidating salsa's memos, which query's memo was
being verified (the innermost `IngredientImpl<...>` on the stack) and which query asked
(the outermost).

Each sample counts for its period, the events (cycles) it stands for: perf's sampling at a
frequency adapts the period to the rate it aims at, so a sample of a thread that just woke
stands for far fewer cycles than one of a thread that has run all along, and counting samples
gives short bursts on many cores, an edit's, more than their share.
"""
import collections, json, re, sys

# Of what is on the stack.
KINDS = [
    ("parse a file", ("SourceFile::parse", "syntax::parsing::parse_text")),
    ("parse a macro expansion", ("token_tree_to_syntax_node",)),
    ("ast id maps", ("AstIdMap>::from_source",)),
    ("span maps", ("real_span_map",)),
    ("item trees", ("hir_def::item_tree::lower", "file_item_tree_query", "block_item_tree_query")),
    ("body syntax (blaze)", ("hir_def::expr_store::body::body_syntax",)),
    ("a body's anchor parsed again", ("BodySyntax>::reparse", "BodySyntax::reparse")),
    ("places in bodies (line tables, panic locations)", ("hir_ty_codegen::places::",)),
    ("bodies and signatures lowered", ("hir_def::expr_store::lower",)),
    ("macro expansion", ("hir_expand::", "mbe::", "proc_macro_api::")),
    ("name resolution", ("hir_def::nameres::collector",)),
    ("inference", ("hir_ty::infer::",)),
    ("MIR building", ("hir_ty::mir::lower",)),
    ("MIR passes and monomorphization", ("hir_ty::mir::monomorphization", "hir_ty::mir::transform", "hir_ty::mir::optimize")),
    ("borrow checking", ("hir_ty_borrowck::",)),
    ("layout", ("hir_ty::layout",)),
    ("code generation", ("hir_ty_codegen::", "cranelift", "llvm")),
    ("debug types described", ("hir_ty_codegen::debuginfo",)),
    ("the store read and written", ("blaze_cas::",)),
    ("diagnostics collected", ("diagnostic", "Diagnostic")),
    ("salsa revalidating memos", ("maybe_changed_after",)),
    ("salsa interning", ("salsa::interned::",)),
]

# Of a sample in salsa, what salsa was doing: the first of these among its frames up to the
# first frame of the compiler's own.
SALSA = [
    ("salsa: memos revalidated", ("maybe_changed_after", "deep_verify", "verify_memo", "shallow_verify", "validate_provisional", "mark_validated_output")),
    ("salsa: values interned", ("salsa::interned::",)),
    ("salsa: memos fetched, stored and the rest", ("salsa::",)),
]

# What a frame itself does, the first that matches.
OWN = [
    ("flat trees built (syntax-tree's flat builder)", ("FlatTreeBuilder", "syntax_tree::flat::FlatTree>::subtree")),
    ("green trees built (syntax-tree's builder and node cache)", ("GreenNodeBuilder", "node_cache::NodeCache", "SyntaxTreeBuilder", "syntax_tree::green::")),
    ("trees walked (syntax-tree's cursor, green or flat)", ("syntax_tree::cursor", "syntax_tree::api", "syntax_tree::flat", "<syntax_tree::")),
    ("lexed and parsed (events)", ("parser::", "LexedStr", "rustc_lexer")),
    ("anchors hashed", ("hir_def::expr_store::body::tree_hash",)),
    ("ast id maps", ("span::ast_id::",)),
    ("span maps", ("span::map::", "real_span_map")),
    ("token trees (syntax to and from)", ("syntax_bridge::", "tt::")),
    ("macro expansion", ("mbe::", "proc_macro_api::", "hir_expand::")),
    ("item trees", ("hir_def::item_tree",)),
    ("body syntax (blaze)", ("hir_def::expr_store::body::",)),
    ("bodies and signatures lowered", ("hir_def::expr_store::lower",)),
    ("name resolution", ("hir_def::nameres",)),
    ("inference and traits", ("hir_ty::infer", "hir_ty::next_solver", "rustc_next_trait_solver", "rustc_type_ir")),
    ("MIR", ("hir_ty::mir",)),
    ("code generation", ("hir_ty_codegen::", "cranelift", "llvm", "blaze_codegen")),
    ("salsa", ("salsa::",)),
]


# "comm pid/tid [cpu] time: period event:"
HEAD = re.compile(r" (\d+\.\d+):\s+(?:(\d+)\s+\S+:)?")


def samples(path, comm, dso=None):
    """Each sample's time, period and frames, innermost first: of processes named `comm`, and
    with `dso` in the path of one of the frames' binaries when it is given. A sample whose line
    gives no period counts for one."""
    frames, keep, of_dso, at, period = [], False, dso is None, 0.0, 1
    for line in open(path, errors="replace"):
        if not line.strip():
            if keep and of_dso and frames:
                yield at, period, frames
            frames, keep, of_dso = [], False, dso is None
        elif not line[0].isspace():
            keep = line.split()[0].startswith(comm)
            head = HEAD.search(line)
            at = float(head[1]) if head else 0.0
            period = int(head[2]) if head and head[2] else 1
        elif keep:
            # "    addr symbol+0xoff (dso)"
            parts = line.strip().split(" ", 1)
            of_dso = of_dso or dso in line
            frames.append(re.sub(r"\+0x[0-9a-f]+ \(.*\)$|\s\(.*\)$", "", parts[1]) if len(parts) > 1 else parts[0])
    if keep and of_dso and frames:
        yield at, period, frames


def own(frames):
    for at, frame in enumerate(frames):
        for label, needles in OWN:
            if any(it in frame for it in needles):
                return salsa_part(frames[at:]) if label == "salsa" else label
    return "other (allocator, kernel, std, unknown)"


def salsa_part(frames):
    """What salsa was doing in a sample whose innermost frame of a kind is salsa's."""
    ours = [label for label, _ in OWN if label != "salsa"]
    run = []
    for frame in frames:
        if any(any(it in frame for it in needles) for label, needles in OWN if label in ours):
            break
        run.append(frame)
    for label, needles in SALSA:
        if any(any(it in frame for it in needles) for frame in run):
            return label
    return SALSA[-1][0]


QUERY = re.compile(r"IngredientImpl<(.+?)> as salsa::ingredient::Ingredient>::maybe_changed_after|IngredientImpl<(.+?)>>::fetch")
NAMED = re.compile(r"(.*)::_::(\w+?)_*Configuration_$")
CALLER = re.compile(r"\bblaze::([\w:]+)")


def query_of(frame):
    """`Body::relative` for `IngredientImpl<<hir_def::...::Body>::relative::_::relative__Configuration_> as ...`."""
    found = QUERY.search(frame)
    if not found:
        return None
    named = NAMED.match(found[1] or found[2])
    if not named:
        return None
    path, name = named.groups()
    return f"{path[1:path.index('>')].rsplit('::', 1)[-1]}::{name}" if path.startswith("<") else name


def caller_of(frames):
    """The innermost frame of the blaze-engine crate itself: the code that asked salsa."""
    for frame in frames:
        found = CALLER.search(frame)
        if found:
            return found[1]
    return "?"


class Fold:
    """The tables of a set of samples, added one at a time."""

    def __init__(self):
        self.total, self.count = 0, 0
        self.on_stack, self.by_own = collections.Counter(), collections.Counter()
        self.verified, self.asked = collections.Counter(), collections.Counter()
        self.callers = collections.Counter()

    def add(self, period, frames):
        self.total += period
        self.count += 1
        joined = ";".join(frames)
        for label, needles in KINDS:
            if any(it in joined for it in needles):
                self.on_stack[label] += period
        kind = own(frames)
        self.by_own[kind] += period
        if kind == SALSA[0][0]:
            queries = [it for it in map(query_of, frames) if it]
            self.verified[queries[0] if queries else "?"] += period
            self.asked[queries[-1] if queries else "?"] += period
            self.callers[caller_of(frames)] += period

    def print(self):
        share = lambda count: f"{100 * count / max(1, self.total):6.2f}%"
        print(f"{self.count} samples, {self.total / 1e9:.1f} G events (cycles) in their periods")
        print("on the stack:")
        for label, _ in KINDS:
            print(f"{share(self.on_stack[label])}  {label}")
        print("by what the innermost frame of a kind does:")
        for label, count in self.by_own.most_common():
            print(f"{share(count)}  {label}")
        if self.verified:
            print("revalidating, by the memo verified (innermost query), of all samples:")
            for label, count in self.verified.most_common(15):
                print(f"{share(count)}  {label}")
            print("revalidating, by the query that asked (outermost), of all samples:")
            for label, count in self.asked.most_common(15):
                print(f"{share(count)}  {label}")
            print("revalidating, by the blaze code that asked, of all samples:")
            for label, count in self.callers.most_common(10):
                print(f"{share(count)}  {label}")


def main():
    comm = sys.argv[sys.argv.index("--comm") + 1] if "--comm" in sys.argv else "blaze"
    dso = sys.argv[sys.argv.index("--dso") + 1] if "--dso" in sys.argv else None
    if "--phases" not in sys.argv:
        fold = Fold()
        for _, period, frames in samples(sys.argv[1], comm, dso):
            fold.add(period, frames)
        fold.print()
        return
    phases = json.load(open(sys.argv[sys.argv.index("--phases") + 1]))["phases"]
    folds = {it["phase"]: Fold() for it in phases}
    for at, period, frames in samples(sys.argv[1], comm, dso):
        for it in phases:
            if it["began"] <= at < it["ended"]:
                folds[it["phase"]].add(period, frames)
                break
    for it in phases:
        print(f"== {it['phase']} ({it['ended'] - it['began']:.1f} s)")
        folds[it["phase"]].print()


if __name__ == "__main__":
    main()
