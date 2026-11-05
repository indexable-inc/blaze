#!/usr/bin/env python3
"""nightly_features.py: one GitHub issue per unstable feature of the pinned nightly, under a
sub-issue tree, so every `#![feature(..)]` blaze has to accept is tracked.

    nightly_features.py '{"dry_run": true}'     what it would create, link and close
    nightly_features.py '{}'                    do it
    nightly_features.py '{"commit": "16d1ffac17ad8b739e32483ac2e65fe7e1804977", "create_limit": 20}'

The JSON is inline, a file's path, or `-` for stdin. Fields:

    commit        the rust-lang/rust commit; empty for library/UPSTREAM's. It has to be
                  library/UPSTREAM's, since the library features are read from library/.
    dry_run       read GitHub and print what would change, change nothing
    create_limit  create at most this many issues this run (0: no limit)
    library       one issue per library feature too (off: one issue for the library as a whole)
    repo          where the issues go (indexable-inc/blaze)

blaze is a superset of the pinned nightly's rustc: every program it accepts, with any
`#![feature(..)]`, builds and behaves the same. The features are:

- language features, from compiler/rustc_feature/src/unstable.rs at the commit (fetched with
  `gh api`): every `(unstable|incomplete|internal, name, "since", issue)` entry and the `///`
  comment above it;
- library features, every `unstable(feature = ..)` and `rustc_const_unstable(feature = ..)`
  attribute (also inside `cfg_attr`) in the sysroot crates under library/ (`CRATES`), but
  coretests and alloctests, the stdarch generators and the vendored crates. A name that is a
  language feature too is counted there.

The tree is a root issue, "Language features" and "Library features" under it, group issues
under "Language features" (by kind, cut into parts of at most 100, GitHub's limit of
sub-issues per issue) and one issue per language feature under its group. "Library features"
is one issue: the library features come with library/, which blaze compiles from source, and
what they need from blaze (stability gates, intrinsics, lang items, `rustc_*` attributes) is
the same for all of them; its body counts them and those. With `library`, it gets groups by
crate and one issue per library feature instead, the same way; without, a rerun closes those
issues. Every
issue has the label nightly-feature (and nightly-lang or nightly-lib) and a hidden marker,
`<!-- nightly-feature:lang:NAME -->`, `lib:NAME`, `group:KEY` or `root`, by which a rerun finds
it. A rerun creates only the issues missing, links the ones not under their group (moving one
whose group changed), rewrites the root's and the groups' titles and bodies, and never
rewrites a feature's issue once it exists: its status line is triage's. A feature that is no
longer unstable at the commit (stabilized or removed) gets a comment saying which, its issue
closed and taken out of its group. It is rerun on main after each toolchain bump merges
(docs/toolchain.md, "The daily bump").

Creating is paced under GitHub's secondary limits (80 a minute, 500 an hour for content):
a create is one GraphQL `createIssue` with its labels and parent; a 403 or 429 waits for
retry-after, or the rate limit's reset, or a minute doubling, and tries again.
"""
import base64, collections, json, math, os, re, subprocess, sys, time, typing
from dataclasses import dataclass, field, fields

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
LIBRARY = os.path.join(ROOT, "library")
UPSTREAM = os.path.join(LIBRARY, "UPSTREAM")
RUST = "rust-lang/rust"
UNSTABLE_RS = "compiler/rustc_feature/src/unstable.rs"
ACCEPTED_RS = "compiler/rustc_feature/src/accepted.rs"
REMOVED_RS = "compiler/rustc_feature/src/removed.rs"
BOOK = "https://doc.rust-lang.org/nightly/unstable-book"
LABELS = {
    "nightly-feature": ("5319e7", "An unstable feature of the pinned nightly blaze has to accept"),
    "nightly-lang": ("1d76db", "A language feature (compiler/rustc_feature/src/unstable.rs)"),
    "nightly-lib": ("0e8a16", "A library feature (#[unstable] in library/)"),
}
MAX_CHILDREN = 100
# Sysroot crate sources under library/, as (directory, crate, group). A feature declared in
# several goes to the first group of `LIB_GROUPS` that has it.
CRATES = [
    ("core/src", "core", "core"),
    ("stdarch/crates/core_arch/src", "core::arch", "arch"),
    ("portable-simd/crates/core_simd/src", "core::simd", "arch"),
    ("alloc/src", "alloc", "alloc"),
    ("std/src", "std", "std"),
    ("backtrace/src", "std (backtrace)", "std"),
    ("proc_macro/src", "proc_macro", "proc_macro"),
    ("test/src", "test", "test"),
    ("std_detect/src", "std_detect", "other"),
    ("panic_abort/src", "panic_abort", "other"),
    ("panic_unwind/src", "panic_unwind", "other"),
    ("unwind/src", "unwind", "other"),
    ("profiler_builtins/src", "profiler_builtins", "other"),
    ("compiler-builtins/compiler-builtins/src", "compiler_builtins", "other"),
    ("sysroot/src", "sysroot", "other"),
    ("windows_link/src", "windows_link", "other"),
    ("windows-sys/src", "windows-sys", "other"),
]
LIB_GROUPS = {
    "core": "core",
    "arch": "core::arch and core::simd (stdarch, portable-simd)",
    "alloc": "alloc",
    "std": "std",
    "proc_macro": "proc_macro",
    "test": "test",
    "other": "the other sysroot crates (std_detect, panic_*, unwind, profiler_builtins, ...)",
}
LANG_KINDS = {
    "internal": "internal: for the standard library and the compiler, never meant to be stabilized",
    "incomplete": "incomplete: rustc warns that the feature is incomplete and may be unsound",
    "unstable": "unstable: the ordinary feature gates, on their way to stabilization",
}
TRIAGE = (
    "Triage: write a program that uses the feature (the unstable book's example and rustc's "
    "tests for it, under tests/ui/feature-gates and tests/ui, are a start), build and run it "
    "with blaze and with the pinned nightly's rustc, and compare standard output, standard "
    "error and exit status. The same: add the program to blaze's tests and close this issue. "
    "Different, or refused by blaze: say how in a comment and change the status line.")
MARKER = re.compile(r"<!-- nightly-feature:(\S+) -->")


class Failed(Exception):
    pass


@dataclass(frozen=True)
class Input:
    # The rust-lang/rust commit, 40 hex digits; empty for library/UPSTREAM's.
    commit: str = ""
    dry_run: bool = False
    # Create at most this many issues this run; 0 for no limit.
    create_limit: int = 0
    # One issue per library feature too, under per-crate groups. Off: one issue for the
    # library as a whole (what blaze has to do for library/ to work).
    library: bool = False
    repo: str = "indexable-inc/blaze"


@dataclass(frozen=True)
class Feature:
    kind: str  # "lang" or "lib"
    name: str
    group: str  # the group's key
    issue: str  # the tracking issue's number, "" for none
    # Language: unstable/incomplete/internal. Library: unstable or const (only
    # rustc_const_unstable).
    gate: str
    since: str = ""
    section: str = ""
    doc: str = ""
    reason: str = ""
    count: int = 0
    paths: tuple = ()
    crates: tuple = ()

    @property
    def key(self):
        return f"{self.kind}:{self.name}"

    @property
    def title(self):
        return f"feature({self.name})" + (" (library)" if self.kind == "lib" else "")


@dataclass
class Group:
    key: str
    parent: str  # the parent's key: "root", "lang" or "lib"
    title: str
    body: str = ""
    members: list = field(default_factory=list)


# ---- reading the features ----

def gh_file(path, commit):
    out = subprocess.run(["gh", "api", f"repos/{RUST}/contents/{path}?ref={commit}", "--jq", ".content"],
                         capture_output=True, text=True)
    if out.returncode:
        raise Failed(f"reading {RUST}'s {path} at {commit}: {out.stderr.strip()}")
    return base64.b64decode(out.stdout).decode()


ENTRY = re.compile(r'^\s*\((unstable|incomplete|internal),\s*(\w+),\s*"([^"]*)",\s*(None|Some\((\d+)\))\s*(?:,[^)]*)?\),')


def lang_features(commit):
    text = gh_file(UNSTABLE_RS, commit)
    start = text.find("declare_features! (")
    if start < 0:
        raise Failed(f"{UNSTABLE_RS} at {commit} has no `declare_features! (`")
    found, doc, section = {}, [], ""
    for line in text[start:].splitlines():
        stripped = line.strip()
        if m := re.match(r"// feature-group-start: (.*)", stripped):
            section = m.group(1).strip()
        if stripped.startswith("///"):
            doc.append(stripped[3:].strip())
            continue
        if m := ENTRY.match(line):
            gate, name, since, _, issue = m.groups()
            found[name] = dict(gate=gate, name=name, since=since, issue=issue or "",
                               section=section, doc=" ".join(doc).strip())
        doc = []
        if stripped == ");":
            break
    if len(found) < 100:
        raise Failed(f"{UNSTABLE_RS} at {commit}: only {len(found)} features parsed")
    return found


ATTR = re.compile(r"(?:#!?\[\s*|cfg_attr\([^\n\[\]]*?,\s*)(unstable|rustc_const_unstable)\s*\(")
STRING_FIELD = r'\b{}\s*=\s*"((?:[^"\\]|\\.)*)"'


def attr_args(text, at):
    """The text from `at` (just past an attribute's `(`) to its closing parenthesis."""
    depth, i, quoted = 1, at, False
    while i < len(text):
        c = text[i]
        if quoted:
            if c == "\\":
                i += 1
            elif c == '"':
                quoted = False
        elif c == '"':
            quoted = True
        elif c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
            if depth == 0:
                return text[at:i]
        i += 1
    return ""


def lib_features():
    seen = collections.defaultdict(lambda: dict(gates=set(), issue="", reason="", count=0,
                                                 paths=[], crates=[], groups=[]))
    for directory, crate, group in CRATES:
        base = os.path.join(LIBRARY, directory)
        if not os.path.isdir(base):
            raise Failed(f"library/{directory} is missing; CRATES names it")
        for top, dirs, files in os.walk(base):
            dirs.sort()
            for name in sorted(files):
                if not name.endswith(".rs"):
                    continue
                path = os.path.join(top, name)
                with open(path, errors="replace") as f:
                    lines = [l for l in f.read().splitlines() if not l.lstrip().startswith("//")]
                text = "\n".join(lines)
                for m in ATTR.finditer(text):
                    args = attr_args(text, m.end())
                    feature = re.search(STRING_FIELD.format("feature"), args)
                    if not feature:
                        continue
                    one = seen[feature.group(1)]
                    one["gates"].add(m.group(1))
                    one["count"] += 1
                    rel = os.path.relpath(path, ROOT)
                    if rel not in one["paths"]:
                        one["paths"].append(rel)
                    if crate not in one["crates"]:
                        one["crates"].append(crate)
                    if group not in one["groups"]:
                        one["groups"].append(group)
                    issue = re.search(STRING_FIELD.format("issue"), args)
                    if issue and issue.group(1).isdigit() and not one["issue"]:
                        one["issue"] = issue.group(1)
                    reason = re.search(STRING_FIELD.format("reason"), args)
                    if reason and not one["reason"]:
                        one["reason"] = reason.group(1).replace('\\"', '"')
    if len(seen) < 100:
        raise Failed(f"library/: only {len(seen)} library features found")
    return seen


def upstream_commit():
    with open(UPSTREAM) as f:
        for line in f:
            if line.startswith("commit "):
                return line.split()[1]
    raise Failed("library/UPSTREAM has no `commit` line")


def nightly_name():
    with open(UPSTREAM) as f:
        for line in f:
            if line.startswith("nightly "):
                return line.split()[1]
    return "the pinned nightly"


# ---- the tree ----

def parts(items, most=MAX_CHILDREN):
    """`items` (sorted) cut into the fewest parts of at most `most`, as even as can be."""
    if not items:
        return []
    count = math.ceil(len(items) / most)
    size = math.ceil(len(items) / count)
    return [items[i:i + size] for i in range(0, len(items), size)]


def library_support():
    """What library/'s sysroot crates ask of the compiler: `#[rustc_intrinsic]` functions,
    `#[lang = ..]` items, and every `rustc_*` attribute with how often it is used."""
    intrinsics, lang_items, attrs = 0, set(), collections.Counter()
    for directory, _, _ in CRATES:
        for top, dirs, files in os.walk(os.path.join(LIBRARY, directory)):
            for name in files:
                if not name.endswith(".rs"):
                    continue
                with open(os.path.join(top, name), errors="replace") as f:
                    text = "\n".join(l for l in f.read().splitlines() if not l.lstrip().startswith("//"))
                intrinsics += len(re.findall(r"#\[rustc_intrinsic\]", text))
                lang_items |= set(re.findall(r'#\[lang = "([^"]+)"\]', text))
                attrs.update(re.findall(r"#!?\[\s*(rustc_[a-z0-9_]+)", text))
    return intrinsics, lang_items, attrs


def build_tree(lang, lib, library):
    features, groups = [], {}

    def add_groups(prefix, parent, label, names, make):
        cut = parts(sorted(names))
        for i, members in enumerate(cut, 1):
            key = f"{prefix}-{i}"
            span = f", part {i} of {len(cut)}: `{members[0]}` to `{members[-1]}`" if len(cut) > 1 else ""
            group = Group(key, parent, f"{label}{span} ({len(members)})")
            for name in members:
                feature = make(name, key)
                group.members.append(feature)
                features.append(feature)
            groups[key] = group

    for gate in LANG_KINDS:
        names = [n for n, f in lang.items() if f["gate"] == gate]
        def make(name, key):
            f = lang[name]
            return Feature("lang", name, key, f["issue"], f["gate"], since=f["since"],
                           section=f["section"], doc=f["doc"])
        add_groups(f"lang-{gate}", "lang", f"Language features: {gate}", names, make)

    if not library:
        return features, groups
    lib_only = {n: v for n, v in lib.items() if n not in lang}
    for gkey, glabel in LIB_GROUPS.items():
        names = [n for n, v in lib_only.items() if v["groups"][0] == gkey]
        def make(name, key):
            v = lib[name]
            gate = "unstable" if "unstable" in v["gates"] else "const"
            return Feature("lib", name, key, v["issue"], gate, reason=v["reason"],
                           count=v["count"], paths=tuple(v["paths"]), crates=tuple(v["crates"]))
        add_groups(f"lib-{gkey}", "lib", f"Library features: {glabel}", names, make)
    return features, groups


def rust_issue(number):
    # redirect.github.com: a link to rust-lang/rust that puts no cross-reference on its issue.
    if not number:
        return "none"
    return f"[rust-lang/rust issue {number}](https://redirect.github.com/{RUST}/issues/{number})"


def book(feature):
    where = "language-features" if feature.kind == "lang" else "library-features"
    return f"{BOOK}/{where}/{feature.name.replace('_', '-')}.html"


def feature_body(f, commit, nightly, lib):
    out = [f"<!-- nightly-feature:{f.key} -->",
           f"`#![feature({f.name})]`, a {'language' if f.kind == 'lang' else 'library'} feature of "
           f"{nightly} (rust-lang/rust `{commit[:10]}`). blaze has to accept every program that "
           f"nightly accepts with it, and build it to behave the same.", ""]
    if f.kind == "lang":
        out += [f"- kind: `{f.gate}` ({LANG_KINDS[f.gate].split(': ', 1)[1]})",
                f"- since: {f.since}",
                f"- declared in: `{UNSTABLE_RS}`, section \"{f.section}\"",
                f"- tracking issue: {rust_issue(f.issue)}",
                f"- unstable book: {book(f)}"]
        if f.name in lib:
            v = lib[f.name]
            out.append(f"- library items under the same gate: {v['count']}, in "
                       + ", ".join(f"`{p}`" for p in v["paths"][:5]) + (" ..." if len(v["paths"]) > 5 else ""))
        out += ["", "rustc's description:", "", "> " + (f.doc or "(none)")]
    else:
        gate = ("`#[unstable]`" if f.gate == "unstable" else
                "`#[rustc_const_unstable]` only: the items are stable, and need the feature only in const contexts")
        shown = ", ".join(f"`{p}`" for p in f.paths[:5]) + (f" and {len(f.paths) - 5} more files" if len(f.paths) > 5 else "")
        out += [f"- declared by: {gate}",
                f"- crates: {', '.join(f.crates)}",
                f"- items: {f.count}, in {shown}",
                f"- tracking issue: {rust_issue(f.issue)}",
                f"- unstable book: {book(f)}"]
        if f.reason:
            out += ["", "The library's reason:", "", "> " + f.reason]
    out += ["", "**blaze status: not yet triaged**", "", TRIAGE, "",
            "Made by `harness/nightly_features.py`, which never rewrites this issue once it exists."]
    return "\n".join(out)


def group_body(group, groups, issues, commit, nightly):
    lang = group.key.startswith("lang")
    out = [f"<!-- nightly-feature:group:{group.key} -->",
           f"{len(group.members)} {'language' if lang else 'library'} features of {nightly} "
           f"(rust-lang/rust `{commit[:10]}`), one sub-issue each."]
    if lang:
        gate = group.key.split("-")[1]
        out += ["", f"Kind {LANG_KINDS[gate]}. From `{UNSTABLE_RS}`."]
    else:
        out += ["", "Most library features come with library/ (blaze builds the standard library "
                "from source) and only need work in blaze where they rest on intrinsics, lang items "
                "or language features. Triage closes the ones that already work."]
    out += ["", "Made by `harness/nightly_features.py`; a rerun rewrites this issue."]
    return "\n".join(out)


def top_body(key, groups, issues, commit, nightly):
    lang = key == "lang"
    mine = [g for g in groups.values() if g.parent == key]
    total = sum(len(g.members) for g in mine)
    out = [f"<!-- nightly-feature:group:{key} -->",
           f"{total} {'language' if lang else 'library'} features of {nightly} (rust-lang/rust "
           f"`{commit[:10]}`), in groups of at most {MAX_CHILDREN}:", ""]
    for g in mine:
        out.append(f"- #{issues['group:' + g.key]['number']} {g.title}" if 'group:' + g.key in issues else f"- {g.title}")
    if lang:
        out += ["", f"From `{UNSTABLE_RS}`: every `declare_features!` entry, grouped by its kind."]
    else:
        out += ["", "From every `#[unstable(feature = ..)]` and `#[rustc_const_unstable(feature = ..)]` in "
                "the sysroot crates under library/, grouped by the crate that declares them (core first, "
                "then core::arch, alloc, std, ...). A name that is also a language feature is counted "
                "there. Most come with library/ and need blaze work only where they rest on intrinsics "
                "or language features; triage closes the ones that already work."]
    out += ["", "Made by `harness/nightly_features.py`; a rerun rewrites this issue."]
    return "\n".join(out)


LIBRARY_TITLE = "Library features: what library/ needs from blaze (stability gates, intrinsics, lang items, rustc_* attributes)"


def library_body(lang, lib, support, commit, nightly):
    """The one library issue, when library features get no issue each."""
    intrinsics, lang_items, attrs = support
    shared = sorted(n for n in lib if n in lang)
    gates = collections.Counter("unstable" if "unstable" in v["gates"] else "const" for n, v in lib.items() if n not in lang)
    crates = collections.Counter(v["groups"][0] for n, v in lib.items() if n not in lang)
    out = ["<!-- nightly-feature:group:lib -->",
           f"The standard library of {nightly} (rust-lang/rust `{commit[:10]}`) declares "
           f"{len(lib) - len(shared)} library features of its own ({gates['unstable']} by `#[unstable]`, "
           f"{gates['const']} by `#[rustc_const_unstable]` alone; by crate: "
           + ", ".join(f"{k} {crates[k]}" for k in LIB_GROUPS if crates[k]) + f"), and {len(shared)} "
           "more under the gate of a language feature, tracked in that feature's issue.",
           "",
           "They get no issue each: they come with library/, which blaze compiles from source, so "
           "a library feature works once blaze compiles the code behind it. What that takes from "
           "blaze is the same for all of them, and it is this issue:",
           "",
           "- **Stability gating.** `#[unstable(feature = ..)]`, `#[rustc_const_unstable]`, "
           "`#[stable]` and `#[rustc_const_stable]` as rustc reads them (`staged_api`): using an "
           "unstable item without its `#![feature]` is rustc's error E0658, with it the item "
           "works, and `#[allow_internal_unstable]` and `#[rustc_allow_const_fn_unstable]` lift "
           "the gate inside a macro or a const fn. A program that names a feature that does not "
           "exist is rustc's error too (E0635).",
           f"- **Intrinsics.** {intrinsics} `#[rustc_intrinsic]` functions in the sysroot crates, "
           "each lowered by the code generator or evaluated by const eval as rustc does.",
           f"- **Lang items.** {len(lang_items)} `#[lang = ..]` items the compiler finds by name "
           "(operators, `Drop`, `Box`, panics, `Sized`, coroutines, ...).",
           f"- **`rustc_*` attributes.** {len(attrs)} distinct ones, by how often library/ uses them:",
           ""]
    out.append(", ".join(f"`{a}` {c}" for a, c in attrs.most_common()))
    out += ["",
            "Close it when blaze compiles the whole of library/ the way rustc does and a program "
            "using any library feature (with its `#![feature]`) builds and behaves the same. "
            "`harness/nightly_features.py '{\"library\": true}'` lists them one issue each instead.",
            "",
            "Made by `harness/nightly_features.py`; a rerun rewrites this issue."]
    return "\n".join(out)


def root_body(groups, issues, tops, library, commit, nightly):
    out = ["<!-- nightly-feature:root -->",
           f"blaze is a superset of the pinned nightly's rustc ({nightly}, rust-lang/rust `{commit}`): "
           "every program that nightly accepts, with any `#![feature(..)]`, has to build under blaze "
           "and behave the same. This tree tracks every unstable feature of that nightly: one issue "
           "per language feature, and "
           + ("one per library feature." if library else
              "one issue for the library features, which come with library/ and need the same work "
              "from blaze (stability gates, intrinsics, lang items, `rustc_*` attributes)."),
           ""]
    for key in ("lang", "lib"):
        top = f"#{issues['group:' + key]['number']} " if 'group:' + key in issues else ""
        out.append(f"- {top}{tops[key]}")
        for g in groups.values():
            if g.parent == key:
                out.append(f"  - #{issues['group:' + g.key]['number']} {g.title}" if 'group:' + g.key in issues else f"  - {g.title}")
    out += ["",
            "**How it is made.** `harness/nightly_features.py` reads the language features from "
            f"`{UNSTABLE_RS}` at the commit and makes an issue for each, labelled `nightly-feature` "
            f"and `nightly-lang`, under a group of at most {MAX_CHILDREN} (GitHub's limit of "
            "sub-issues). It reads the library features from the `#[unstable]` and "
            "`#[rustc_const_unstable]` attributes of the sysroot crates in library/ for the library "
            "issue (`'{\"library\": true}'` makes one issue each instead). Each issue carries a hidden "
            "marker, so a rerun creates only what is missing and links what is not under its group. "
            "It is rerun after each toolchain bump: a feature stabilized or removed in the new nightly "
            "gets a comment and its issue closed; a new one gets an issue.",
            "",
            "**How status is decided.** Every feature issue starts at \"blaze status: not yet triaged\". "
            "Triage builds and runs a program using the feature with blaze and with the pinned rustc and "
            "compares their output and exit status. The same: the program becomes a test and the issue "
            "closes. Different, or refused: the issue stays open saying how.",
            "",
            "Made by `harness/nightly_features.py`; a rerun rewrites this issue."]
    return "\n".join(out)


# ---- GitHub ----

class Pace:
    """Spaces mutations: at least `gap` seconds apart, at most `minute` in any 60 s and `hour`
    in any hour."""

    def __init__(self, gap=2.5, minute=70, hour=450):
        self.gap, self.minute, self.hour, self.times = gap, minute, hour, collections.deque()

    def wait(self):
        while True:
            now = time.time()
            while self.times and now - self.times[0] > 3600:
                self.times.popleft()
            last_minute = sum(1 for t in self.times if now - t < 60)
            waits = [0.0]
            if self.times:
                waits.append(self.times[-1] + self.gap - now)
            if last_minute >= self.minute:
                waits.append(min(t for t in self.times if now - t < 60) + 60 - now)
            if len(self.times) >= self.hour:
                waits.append(self.times[0] + 3600 - now)
            pause = max(waits)
            if pause <= 0:
                self.times.append(now)
                return
            if pause > 30:
                print(f"  pacing: waiting {pause:.0f} s ({len(self.times)} changes in the last hour)", flush=True)
            time.sleep(pause)


class GitHub:
    def __init__(self, repo, dry_run):
        self.repo, self.dry_run, self.pace, self.changes = repo, dry_run, Pace(), 0

    def call(self, method, path, body=None, mutate=False):
        """`gh api` once per try; returns (status, parsed body). Waits out rate limits."""
        if mutate:
            if self.dry_run:
                raise Failed(f"dry run: {method} {path} would change GitHub")
            self.pace.wait()
            self.changes += 1
        backoff = 60
        for attempt in range(10):
            argv = ["gh", "api", "-i", "-X", method, path, "-H", "Accept: application/vnd.github+json"]
            if body is not None:
                argv += ["--input", "-"]
            done = subprocess.run(argv, input=json.dumps(body) if body is not None else None,
                                  capture_output=True, text=True)
            head, _, rest = done.stdout.partition("\r\n\r\n") if "\r\n\r\n" in done.stdout else done.stdout.partition("\n\n")
            lines = head.splitlines()
            status = int(lines[0].split()[1]) if lines and lines[0].startswith("HTTP/") else 0
            headers = {k.strip().lower(): v.strip() for k, _, v in (l.partition(":") for l in lines[1:])}
            try:
                data = json.loads(rest) if rest.strip() else None
            except json.JSONDecodeError:
                data = rest
            limited = status in (403, 429) and (
                "retry-after" in headers or headers.get("x-ratelimit-remaining") == "0"
                or "rate limit" in str(data).lower())
            if path == "graphql" and status == 200 and isinstance(data, dict) and data.get("errors"):
                if any("rate" in str(e).lower() for e in data["errors"]):
                    limited = True
            if limited:
                if "retry-after" in headers:
                    pause = int(headers["retry-after"]) + 1
                elif headers.get("x-ratelimit-remaining") == "0":
                    pause = max(5, int(headers.get("x-ratelimit-reset", time.time() + 60)) - time.time() + 2)
                else:
                    pause, backoff = backoff, min(backoff * 2, 900)
                print(f"  rate limited ({status}) on {method} {path}; waiting {pause:.0f} s", flush=True)
                time.sleep(pause)
                continue
            if status >= 500 or status == 0:
                if mutate:
                    # The change may have gone through: the caller checks before trying again.
                    raise Failed(f"{method} {path}: HTTP {status or 'none'}: {done.stderr.strip()[-300:]}")
                time.sleep(min(5 * 2 ** attempt, 120))
                continue
            return status, data
        raise Failed(f"{method} {path}: still failing after 10 tries")

    def ok(self, method, path, body=None, mutate=False, accept=(200, 201, 204)):
        status, data = self.call(method, path, body, mutate)
        if status not in accept:
            raise Failed(f"{method} {path}: HTTP {status}: {str(data)[:400]}")
        return data

    def pages(self, path):
        out, page = [], 1
        sep = "&" if "?" in path else "?"
        while True:
            batch = self.ok("GET", f"{path}{sep}per_page=100&page={page}")
            out += batch
            if len(batch) < 100:
                return out
            page += 1

    def graphql(self, query, variables, mutate=False):
        data = self.ok("POST", "graphql", {"query": query, "variables": variables}, mutate=mutate)
        if data.get("errors"):
            raise Failed(f"graphql: {json.dumps(data['errors'])[:400]}")
        return data["data"]


def ensure_labels(gh):
    have = {l["name"]: l for l in gh.pages(f"repos/{gh.repo}/labels")}
    ids = {}
    for name, (color, about) in LABELS.items():
        if name not in have:
            print(f"label {name}: create")
            if gh.dry_run:
                ids[name] = None
                continue
            have[name] = gh.ok("POST", f"repos/{gh.repo}/labels",
                               {"name": name, "color": color, "description": about}, mutate=True)
        ids[name] = have[name]["node_id"]
    return ids


def existing(gh):
    """Our issues by marker key; a key on two issues keeps the oldest and names the other."""
    found = {}
    for issue in gh.pages(f"repos/{gh.repo}/issues?labels=nightly-feature&state=all&sort=created&direction=asc"):
        if "pull_request" in issue:
            continue
        m = MARKER.search(issue.get("body") or "")
        if not m:
            continue
        if m.group(1) in found:
            print(f"  duplicate: #{issue['number']} has the marker of #{found[m.group(1)]['number']}")
            continue
        found[m.group(1)] = issue
    return found


def recent_with(gh, key):
    """The issue with marker `key` among the newest, after a create whose answer was lost."""
    for issue in gh.ok("GET", f"repos/{gh.repo}/issues?labels=nightly-feature&state=all&sort=created&direction=desc&per_page=50"):
        m = MARKER.search(issue.get("body") or "")
        if m and m.group(1) == key:
            return issue
    return None


CREATE = """mutation($repo: ID!, $title: String!, $body: String!, $labels: [ID!], $parent: ID) {
  createIssue(input: {repositoryId: $repo, title: $title, body: $body, labelIds: $labels, parentIssueId: $parent}) {
    issue { id databaseId number url }
  }
}"""


def create(gh, repo_id, key, title, body, labels, parent):
    """Creates an issue (labelled, under `parent`, an issue dict or None); returns it as REST would."""
    variables = {"repo": repo_id, "title": title, "body": body, "labels": labels,
                 "parent": parent["node_id"] if parent else None}
    for attempt in range(3):
        try:
            made = gh.graphql(CREATE, variables, mutate=True)["createIssue"]["issue"]
            return {"node_id": made["id"], "id": made["databaseId"], "number": made["number"],
                    "html_url": made["url"], "title": title, "body": body, "state": "open"}
        except Failed as error:
            time.sleep(10)
            if found := recent_with(gh, key):
                return found
            if attempt == 2:
                raise
            print(f"  create {key} failed, trying again: {error}", flush=True)


def children(gh, number):
    return {c["id"]: c for c in gh.pages(f"repos/{gh.repo}/issues/{number}/sub_issues")}


def link(gh, parent, child):
    path = f"repos/{gh.repo}/issues/{parent['number']}/sub_issues"
    status, data = gh.call("POST", path, {"sub_issue_id": child["id"]}, mutate=True)
    if status == 422 and "parent" in str(data).lower():
        status, data = gh.call("POST", path, {"sub_issue_id": child["id"], "replace_parent": True}, mutate=True)
    if status not in (200, 201):
        raise Failed(f"linking #{child['number']} under #{parent['number']}: HTTP {status}: {str(data)[:300]}")


def why_gone(name, kind, commit):
    """"stabilized" or "removed" (or "no longer unstable") for a feature gone from the commit."""
    if kind == "lang":
        if re.search(rf"\(\s*accepted,\s*{re.escape(name)}\b", gh_file(ACCEPTED_RS, commit)):
            return "stabilized"
        if re.search(rf"\(\s*removed,\s*{re.escape(name)}\b", gh_file(REMOVED_RS, commit)):
            return "removed"
        return "no longer unstable"
    for top, _, files in os.walk(LIBRARY):
        if "/vendor" in top:
            continue
        for f in files:
            if f.endswith(".rs"):
                with open(os.path.join(top, f), errors="replace") as h:
                    if re.search(rf'stable\s*\(\s*feature\s*=\s*"{re.escape(name)}"', h.read()):
                        return "stabilized"
    return "removed or renamed"


# ---- main ----

def run(given):
    commit = given.commit or upstream_commit()
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise Failed(f"field commit is 40 hex digits, not {commit!r}")
    if commit != upstream_commit():
        raise Failed(f"commit {commit} is not library/UPSTREAM's ({upstream_commit()}): the library "
                     "features come from library/, so move it first (library.py merge)")
    nightly = nightly_name()
    lang, lib = lang_features(commit), lib_features()
    features, groups = build_tree(lang, lib, given.library)
    counts = {"lang": sum(1 for f in features if f.kind == "lang"),
              "lib": sum(1 for n in lib if n not in lang)}
    gates = collections.Counter(f.gate for f in features if f.kind == "lang")
    print(f"{nightly}, rust-lang/rust {commit}")
    print(f"language features: {counts['lang']} ({', '.join(f'{v} {k}' for k, v in gates.items())})")
    print(f"library features: {counts['lib']} (and {sum(1 for n in lib if n in lang)} that are language features too)"
          + ("" if given.library else "; one issue for them all"))
    for g in groups.values():
        print(f"  {g.key:18} {len(g.members):4}  {g.title}")
        if len(g.members) > MAX_CHILDREN:
            raise Failed(f"group {g.key} has {len(g.members)} members, over {MAX_CHILDREN}")

    gh = GitHub(given.repo, given.dry_run)
    repo_id = gh.ok("GET", f"repos/{gh.repo}")["node_id"]
    labels = ensure_labels(gh)
    issues = existing(gh)
    print(f"existing issues: {len(issues)}")
    made, linked, closed, budget = 0, 0, 0, given.create_limit or math.inf
    plan = collections.Counter()

    def ensure(key, title, body, label_names, parent_key):
        nonlocal made
        if key in issues:
            return issues[key]
        plan["create"] += 1
        if gh.dry_run or made >= budget:
            return None
        parent = issues.get(parent_key) if parent_key else None
        if parent_key and parent is None:
            return None
        issue = create(gh, repo_id, key, title, body, [labels[n] for n in label_names], parent)
        issue["_new"] = True
        issues[key] = issue
        made += 1
        print(f"  #{issue['number']} {title}", flush=True)
        return issue

    tops = {"lang": f"Language features ({counts['lang']})",
            "lib": f"Library features ({counts['lib']})" if given.library else LIBRARY_TITLE}
    support = None if given.library else library_support()

    def top(key, known):
        if key == "lib" and not given.library:
            return library_body(lang, lib, support, commit, nightly)
        return top_body(key, groups, known, commit, nightly)

    ensure("root", "Nightly features: every #![feature] of the pinned nightly works under blaze",
           root_body(groups, issues, tops, given.library, commit, nightly), ["nightly-feature"], None)
    for key, title in tops.items():
        ensure(f"group:{key}", title, top(key, {}), ["nightly-feature", f"nightly-{key}"], "root")
    for g in groups.values():
        ensure(f"group:{g.key}", g.title, group_body(g, groups, issues, commit, nightly),
               ["nightly-feature"], f"group:{g.parent}")
    for f in features:
        ensure(f.key, f.title, feature_body(f, commit, nightly, lib),
               ["nightly-feature", f"nightly-{f.kind}"], f"group:{f.group}")

    # Every issue under its parent; the ones created this run already are.
    want = {"group:lang": "root", "group:lib": "root"}
    want |= {f"group:{g.key}": f"group:{g.parent}" for g in groups.values()}
    want |= {f.key: f"group:{f.group}" for f in features}
    under = {}
    for parent_key in sorted(set(want.values())):
        parent = issues.get(parent_key)
        if parent and not parent.get("_new"):
            under[parent_key] = set(children(gh, parent["number"]))
    for key, parent_key in want.items():
        issue, parent = issues.get(key), issues.get(parent_key)
        if not issue or not parent or issue.get("_new"):
            continue
        if issue["id"] in under.get(parent_key, set()):
            continue
        plan["link"] += 1
        if not gh.dry_run:
            link(gh, parent, issue)
            linked += 1

    # Titles and bodies of the root and the groups, now that every number is known.
    rewrite = {"root": ("Nightly features: every #![feature] of the pinned nightly works under blaze",
                        root_body(groups, issues, tops, given.library, commit, nightly))}
    for key, title in tops.items():
        rewrite[f"group:{key}"] = (title, top(key, issues))
    for g in groups.values():
        rewrite[f"group:{g.key}"] = (g.title, group_body(g, groups, issues, commit, nightly))
    for key, (title, body) in rewrite.items():
        issue = issues.get(key)
        if issue and (issue.get("title") != title or (issue.get("body") or "") != body):
            plan["rewrite"] += 1
            if not gh.dry_run:
                gh.ok("PATCH", f"repos/{gh.repo}/issues/{issue['number']}", {"title": title, "body": body}, mutate=True)

    # Features gone from the commit, and groups no longer used.
    current = {f.key for f in features} | set(rewrite)
    for key, issue in issues.items():
        if key in current or issue.get("state") != "open":
            continue
        kind, _, name = key.partition(":")
        if not given.library and (kind == "lib" or key.startswith("group:lib-")):
            why = "one library issue"
            where = f"#{issues['group:lib']['number']}" if "group:lib" in issues else "the library issue"
            note = (f"Library features no longer get an issue each: they come with library/, and what "
                    f"blaze has to do for them is tracked as one issue, {where}. Closed by "
                    "`harness/nightly_features.py`.")
        elif kind in ("lang", "lib"):
            why = why_gone(name, kind, commit)
            note = (f"`{name}` is {why} at rust-lang/rust `{commit}` ({nightly}): it is no longer an "
                    "unstable feature, so it leaves this tree. Closed by `harness/nightly_features.py`.")
        else:
            why = "unused"
            note = f"No longer a group of the tree at `{commit}`. Closed by `harness/nightly_features.py`."
        plan[f"close ({why})"] += 1
        print(f"  close #{issue['number']} {issue.get('title')}: {why}")
        if gh.dry_run:
            continue
        gh.ok("POST", f"repos/{gh.repo}/issues/{issue['number']}/comments", {"body": note}, mutate=True)
        reason = "completed" if why == "stabilized" else "not_planned"
        gh.ok("PATCH", f"repos/{gh.repo}/issues/{issue['number']}", {"state": "closed", "state_reason": reason}, mutate=True)
        holder = next((issues[k] for k, ids in under.items() if issue["id"] in ids), None)
        if holder is None:
            status, parent = gh.call("GET", f"repos/{gh.repo}/issues/{issue['number']}/parent")
            holder = parent if status == 200 and isinstance(parent, dict) else None
        if holder:
            gh.ok("DELETE", f"repos/{gh.repo}/issues/{holder['number']}/sub_issue",
                  {"sub_issue_id": issue["id"]}, mutate=True)
        closed += 1

    if gh.dry_run:
        print("dry run: " + (", ".join(f"{v} to {k}" for k, v in plan.items()) or "nothing to change"))
        return
    left = plan["create"] - made
    print(f"created {made}, linked {linked}, closed {closed}, {gh.changes} changes in all"
          + (f"; {left} not created (create_limit): run it again" if left else ""))
    if "root" in issues:
        print(f"root: {issues['root']['html_url']}")


def typed(cls, text):
    """`cls` from the JSON `text` (inline, a path, or `-`); exits naming the field that is wrong."""
    if text == "-":
        text = sys.stdin.read()
    elif text and not text.lstrip().startswith("{"):
        text = open(text).read()
    try:
        value = json.loads(text or "{}")
    except json.JSONDecodeError as e:
        sys.exit(f"nightly_features.py: the input is not JSON: {e}")
    if not isinstance(value, dict):
        sys.exit("nightly_features.py: the input is a JSON object")
    known = {f.name for f in fields(cls)}
    if unknown := sorted(set(value) - known):
        sys.exit(f"nightly_features.py: no field {', '.join(unknown)} (fields: {', '.join(sorted(known))})")
    hints = typing.get_type_hints(cls)
    for name, given in value.items():
        if type(given) is not hints[name]:
            sys.exit(f"nightly_features.py: field {name} is a {hints[name].__name__}, not {given!r}")
    if value.get("create_limit", 0) < 0:
        sys.exit("nightly_features.py: field create_limit is 0 or more")
    return cls(**value)


def main():
    argv = sys.argv[1:]
    if len(argv) > 1:
        sys.exit(f"usage: nightly_features.py '<json>'\n\n{__doc__}")
    given = typed(Input, argv[0] if argv else "")
    try:
        run(given)
    except Failed as error:
        sys.exit(f"error: nightly_features: {error}")


if __name__ == "__main__":
    main()
