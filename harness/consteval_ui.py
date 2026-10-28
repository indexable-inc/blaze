#!/usr/bin/env python3
"""consteval_ui.py: blaze's const evaluator against rustc's own tests of constants.

usage: consteval_ui.py CONFIG

CONFIG is one JSON object, inline, in a file, or `-` for standard input:

    {"rust": "<rust checkout>", "bin": "<blaze-engine>", "work": "<dir>", "jobs": 8,
     "out": "tools/bench/results/<date>-consteval", "run_pass": true,
     "min_evaluation": 0, "min_run_pass": 0}

`rust` and `bin` are required; `bin` defaults to nothing, so a run names the blaze it scores.
The tests are those of `tests/ui/consts` and `tests/ui/const-generics`, at the commit of the
pinned nightly (CI's checkout, .github/actions/rust-tests). They are scored two ways:

  evaluation: each revision of each test is a unit, and what rustc says of it is what the test
      expects of the pinned nightly: a run-, build- or check-pass test evaluates every constant,
      and a test with expected output fails to evaluate the items its `error[E0080]`s name
      ("evaluation of `NAME` failed"). `blaze-engine consteval` evaluates every free constant
      and static of the file (rustc evaluates those in every build), and the constants of
      inherent impls; a unit passes when the items blaze fails to evaluate are the ones rustc
      names. A unit rustc's verdict cannot be compared with is out of scope, with a counted
      reason: an auxiliary crate, a `-Z` flag, an error other than const evaluation, an error
      of a generic or anonymous constant's instance, or of an item blaze does not evaluate.
  values (`run_pass`): the run-pass tests of those directories through uitest.py: rustc and
      blaze build each, and the two programs must exit and print the same. Their asserts
      read the constants blaze evaluated.

Writes `summary.md` (pass rates, every difference grouped by kind, the out-of-scope reasons)
and `units.json` to `out`, and prints the summary. Exits 1 when blaze panics on a file, or
when fewer units pass than `min_evaluation` or fewer run-pass tests match than `min_run_pass`
(the numbers the release train holds, docs/harness.md).
"""
import collections, concurrent.futures, dataclasses, json, os, re, shlex, subprocess, sys, typing

HERE = os.path.dirname(os.path.abspath(__file__))
DIRS = ("consts", "const-generics")
LINE = r"^//@ *(?:\[(?P<rev>[\w,-]+)\] *)?"
DIRECTIVE = re.compile(LINE + r"(?P<name>[\w-]+)(?: *: *| +|$)(?P<value>.*)$", re.M)
PASS = {"run-pass", "build-pass", "check-pass"}
AUX = {"aux-build", "aux-crate", "proc-macro", "aux-bin", "aux-codegen-backend"}
# Flags that do not change what a constant evaluates to. blaze's trait solver is the next one.
INERT = re.compile(r"^(-[AWDF] ?\S*|--cap-lints(=| )\S+|-g|-C ?\S+|-O|--crate-type(=| )\S+|--check-cfg(=| )\S+"
                   r"|--error-format(=| )\S+|--diagnostic-width(=| )\S+|--crate-name(=| )\S+|--emit(=| )\S+"
                   r"|--remap-path-prefix(=| )\S+|--edition(=| )\S+|--cfg(=| )\S+|--test"
                   r"|-Z ?(ui-testing|deduplicate-diagnostics|track-diagnostics|treat-err-as-bug|"
                   r"verbose-internals|write-long-types-to-disk|mir-opt-level|validate-mir|"
                   r"translate-remapped-path-to-local-path|next-solver)(=\S+)?)$")
ERROR = re.compile(r"^error(\[(?P<code>E\d{4})\])?: (?P<message>.*)$", re.M)
FAILED = re.compile(r"evaluation of `(?P<name>[^`]+)` failed")
# Errors that follow from one rustc already reported for the constant.
FOLLOW_ON = ("erroneous constant encountered", "erroneous static encountered", "aborting due to",
             "could not evaluate constant pattern", "referenced constant has errors")


@dataclasses.dataclass(frozen=True)
class Config:
    rust: str
    bin: str
    work: str = os.path.join(os.environ.get("TMPDIR", "/tmp"), "consteval-ui")
    jobs: int = max(1, (os.cpu_count() or 2) // 2)
    out: str = ""
    run_pass: bool = True
    min_evaluation: int = 0
    min_run_pass: int = 0


def config(argument: str) -> Config:
    """The one JSON value of the command line, as a `Config`."""
    text = sys.stdin.read() if argument == "-" else open(argument).read() if os.path.isfile(argument) else argument
    try:
        value = json.loads(text)
    except json.JSONDecodeError as e:
        sys.exit(f"consteval_ui.py: the configuration is not JSON: {e}")
    if not isinstance(value, dict):
        sys.exit("consteval_ui.py: the configuration is a JSON object")
    fields = {f.name: f for f in dataclasses.fields(Config)}
    for key, item in value.items():
        if key not in fields:
            sys.exit(f"consteval_ui.py: `{key}` is not a field of the configuration ({', '.join(fields)})")
        want = typing.get_type_hints(Config)[key]
        if type(item) is not want:
            sys.exit(f"consteval_ui.py: `{key}` is a {want.__name__}, not {json.dumps(item)}")
    for key, f in fields.items():
        if key not in value and f.default is dataclasses.MISSING:
            sys.exit(f"consteval_ui.py: `{key}` is required")
    return Config(**value)


@dataclasses.dataclass
class Unit:
    """One test in one revision."""
    test: str
    path: str
    revision: str | None
    edition: str = "2015"
    cfgs: list = dataclasses.field(default_factory=list)
    # What rustc fails to evaluate, by the names its errors give; None when out of scope.
    expected: set | None = None
    out: str | None = None
    found: dict | None = None
    verdict: str = ""
    differences: list = dataclasses.field(default_factory=list)

    @property
    def label(self):
        return f"{self.test}#{self.revision}" if self.revision else self.test


def collect(root):
    """Every test file of DIRS, relative to tests/ui."""
    ui = os.path.join(root, "tests", "ui")
    for top in DIRS:
        for directory, subdirs, files in os.walk(os.path.join(ui, top)):
            subdirs[:] = sorted(d for d in subdirs if d != "auxiliary")
            for name in sorted(files):
                if name.endswith(".rs"):
                    path = os.path.join(directory, name)
                    yield os.path.relpath(path, ui), path


def units_of(test, path):
    """The units of one test file, each with what rustc says of it or why it is out of scope."""
    text = open(path, errors="replace").read()
    directives = [(m["rev"], m["name"], m["value"].strip()) for m in DIRECTIVE.finditer(text)]
    revisions = next((v.replace(",", " ").split() for r, n, v in directives if n == "revisions" and r is None), [None])
    units = []
    for revision in revisions:
        unit = Unit(test, path, revision)
        if revision:
            unit.cfgs.append(revision)
        mine = [(n, v) for r, n, v in directives if r is None or revision in r.split(",")]
        unit.cfgs += checks_of([f for n, v in mine if n == "compile-flags" for f in flags_of(v)])
        passes = False
        for name, value in mine:
            if name == "edition":
                unit.edition = value.split("..")[0].strip() or "2015"
            elif name in AUX:
                unit.out = "an auxiliary crate"
            elif name in PASS:
                passes = True
            elif name == "compile-flags":
                for flag in flags_of(value):
                    if flag.startswith("--edition"):
                        unit.edition = flag.replace("=", " ").split()[1]
                    elif flag.startswith("--cfg"):
                        unit.cfgs.append(flag.replace("=", " ", 1).split(None, 1)[1].strip())
                    elif not INERT.match(flag):
                        unit.out = unit.out or f"the flag `{flag.split('=')[0]}`"
            elif name == "needs-asm-support" or name.startswith("only-") and name not in ("only-64bit", "only-x86_64", "only-linux", "only-unix"):
                unit.out = unit.out or f"`{name}`"
        if not unit.out:
            unit.expected = expected_of(unit, path, passes)
        units.append(unit)
    return units


def checks_of(flags):
    """The cfgs of the checks rustc builds with: compiletest does not optimize, so debug
    assertions and overflow checks are on unless the flags say otherwise."""
    setting = lambda value: value.split("=", 1)[1].strip() in ("", "y", "yes", "on", "true") if "=" in value else True
    debug = overflow = None
    optimized = False
    for flag in flags:
        option = re.sub(r"^-C ?", "", flag) if flag.startswith("-C") else None
        if flag == "-O" or option and option.startswith("opt-level") and option.split("=", 1)[-1].strip() not in ("0", ""):
            optimized = True
        elif option and option.startswith("debug-assertions"):
            debug = setting(option)
        elif option and option.startswith("overflow-checks"):
            overflow = setting(option)
    debug = not optimized if debug is None else debug
    overflow = debug if overflow is None else overflow
    return [name for name, on in (("debug_assertions", debug), ("overflow_checks", overflow)) if on]


def flags_of(text):
    """The flags of a `compile-flags` line, an option and its separate value as one."""
    words, flags = shlex.split(text), []
    while words:
        word = words.pop(0)
        if word in ("-C", "-Z", "--cfg", "--check-cfg", "--edition", "--crate-type", "--cap-lints", "-A", "-W", "-D",
                    "-F", "--crate-name", "--extern", "-L", "-l", "--emit", "--target", "--error-format") and words:
            word = f"{word} {words.pop(0)}"
        flags.append(word)
    return flags


def expected_of(unit, path, passes):
    """The names of what rustc fails to evaluate in `unit`, from its expected output; sets
    `unit.out` when that cannot be compared."""
    base = path[:-3]
    stderr = next((f for f in ([f"{base}.{unit.revision}.stderr"] if unit.revision else []) + [f"{base}.stderr"]
                   if os.path.exists(f)), None)
    if stderr is None:
        if passes:
            return set()
        unit.out = "no expected output, and not a passing test"
        return None
    text = open(stderr, errors="replace").read()
    names = set()
    blocks = [m for m in ERROR.finditer(text)]
    for i, m in enumerate(blocks):
        message = m["message"]
        if message.startswith(FOLLOW_ON):
            continue
        if m["code"] != "E0080":
            unit.out = f"rustc reports {m['code'] or 'an error'} besides const evaluation"
            return None
        end = blocks[i + 1].start() if i + 1 < len(blocks) else len(text)
        found = FAILED.findall(text[m.start():end])
        if not found:
            unit.out = "an E0080 that names no constant"
            return None
        names.update(found)
    if any("<" in name or "{" in name for name in names):
        unit.out = "an error of a generic or anonymous constant's instance"
        return None
    return names


def evaluate(binary, units, jobs, work):
    """`blaze-engine consteval` on each unit, by edition, `jobs` processes at a time."""
    os.makedirs(work, exist_ok=True)
    by_edition = collections.defaultdict(list)
    for unit in units:
        by_edition[unit.edition].append(unit)
    batches = []
    for edition, group in by_edition.items():
        size = max(1, -(-len(group) // max(1, jobs // len(by_edition) or 1)))
        batches += [(edition, group[i:i + size]) for i in range(0, len(group), size)]

    def run(batch):
        edition, group = batch
        lines = [u.path + ("\t" + " ".join(u.cfgs) if u.cfgs else "") for u in group]
        try:
            done = subprocess.run([binary, "consteval", "--edition", edition], input="\n".join(lines) + "\n",
                                  capture_output=True, text=True, timeout=1800, cwd=work)
        except subprocess.TimeoutExpired:
            return [(u, {"panic": "timed out"}) for u in group]
        found = [json.loads(line) for line in done.stdout.splitlines() if line.startswith("{")]
        if len(found) != len(group):
            tail = " ".join(done.stderr.strip().splitlines()[-3:])[-300:]
            return [(u, found[i] if i < len(found) else {"panic": f"blaze-engine stopped: {tail}"})
                    for i, u in enumerate(group)]
        return list(zip(group, found))

    with concurrent.futures.ThreadPoolExecutor(max(1, jobs)) as pool:
        for pairs in pool.map(run, batches):
            for unit, found in pairs:
                unit.found = found


def kind_of_error(error):
    """What a failed evaluation of blaze is, without the parts that differ from item to item."""
    error = re.sub(r"WithStack \{ error: ", "", error)
    error = re.sub(r", stack: \[.*", "", error, flags=re.S)
    error = re.sub(r"-?\b\d+\b", "N", error)
    error = re.sub(r"Idx::<[^>]*>\(N\)|\w+Id\(\w+\)", "Id", error)
    return error[:120]


def kind_of_rustc(test_path, revision, name):
    """The first words of rustc's error for `name`, which say what kind of failure it is."""
    base = test_path[:-3]
    for stderr in ([f"{base}.{revision}.stderr"] if revision else []) + [f"{base}.stderr"]:
        if os.path.exists(stderr):
            text = open(stderr, errors="replace").read()
            blocks = list(ERROR.finditer(text))
            for i, m in enumerate(blocks):
                end = blocks[i + 1].start() if i + 1 < len(blocks) else len(text)
                if f"evaluation of `{name}` failed" in text[m.start():end]:
                    words = re.sub(r"-?\b\d+\b", "N", re.sub(r"`[^`]*`", "`_`", m["message"])).split()
                    return " ".join(words[:6])
    return "?"


def score(units):
    for unit in units:
        if unit.out:
            unit.verdict = "out"
            continue
        found = unit.found or {}
        if "panic" in found:
            unit.verdict = "panic"
            unit.differences.append(("blaze panicked", found["panic"][:200]))
            continue
        items = found.get("items", [])
        known = {it["name"] for it in items}
        missing = sorted(unit.expected - known)
        if missing:
            unit.verdict = "out"
            unit.out = "rustc's error names an item blaze does not evaluate (in a body, say)"
            continue
        failed = {it["name"]: it.get("error", "") for it in items if not it["ok"] and (it["eager"] or it["name"] in unit.expected)}
        for name in sorted(set(failed) - unit.expected):
            # rustc reports one that fails through a constant it named as "erroneous constant
            # encountered", which FOLLOW_ON does not count.
            through = re.search(r'ConstEvalError\("([^"]+)"', failed[name])
            if through and through[1].rsplit("::", 1)[-1] in {e.rsplit("::", 1)[-1] for e in unit.expected}:
                continue
            unit.differences.append(("blaze fails where rustc evaluates", f"{name}: {kind_of_error(failed[name])}"))
        for name in sorted(unit.expected - set(failed)):
            unit.differences.append(("rustc fails where blaze evaluates", f"{name}: {kind_of_rustc(unit.path, unit.revision, name)}"))
        unit.verdict = "different" if unit.differences else "same"


def run_pass(cfg):
    """The run-pass tests of DIRS through uitest.py, as its results."""
    results = []
    for top in DIRS:
        out = os.path.join(cfg.work, f"uitest-{top}.json")
        env = dict(os.environ, BLAZE_BIN=os.path.abspath(cfg.bin))
        subprocess.run([sys.executable, os.path.join(HERE, "uitest.py"), cfg.rust, "--filter", f"{top}/", "--jobs",
                        str(max(1, cfg.jobs // 2)), "--work", os.path.join(cfg.work, f"ui-{top}"), "--json", out],
                       env=env, stdout=sys.stderr)
        if os.path.exists(out):
            results += [r for r in json.load(open(out)) if r["test"].startswith(f"{top}/")]
    return results


def summary(units, ran):
    lines = []
    scope = [u for u in units if u.verdict != "out"]
    same = sum(u.verdict == "same" for u in scope)
    lines.append("# consteval_ui.py\n")
    lines.append(f"Evaluation: **{same} of {len(scope)}** units the same as rustc "
                 f"({100 * same / max(1, len(scope)):.1f}%), of {len(units)} units in {len({u.test for u in units})} tests "
                 f"of tests/ui/{{{','.join(DIRS)}}}; {len(units) - len(scope)} out of scope.\n")
    if ran is not None:
        counted = [r for r in ran if r["outcome"] != "excluded"]
        match = sum(r["outcome"] == "match" for r in counted)
        lines.append(f"Values (run-pass, uitest.py): **{match} of {len(counted)}** match "
                     f"({100 * match / max(1, len(counted)):.1f}%); {len(ran) - len(counted)} left out.\n")
    kinds = collections.Counter()
    examples = collections.defaultdict(list)
    for unit in scope:
        for kind, detail in unit.differences:
            head = detail.split(": ", 1)[-1]
            kinds[(kind, head)] += 1
            if unit.label not in examples[(kind, head)]:
                examples[(kind, head)].append(unit.label)
    lines.append("## Differences of evaluation, largest class first\n")
    lines.append("An item is one constant or static; a unit, one revision of a test.\n")
    lines.append("| items | units | kind | what | e.g. |\n|---|---|---|---|---|")
    for (kind, head), n in kinds.most_common():
        shown = examples[(kind, head)]
        lines.append(f"| {n} | {len(shown)} | {kind} | {head.replace('|', '/')} | {', '.join(shown[:3])} |")
    if ran is not None:
        lines.append("\n## Differences of values (run-pass)\n")
        for r in ran:
            if r["outcome"] not in ("match", "excluded"):
                revision = f"#{r['revision']}" if r.get("revision") else ""
                lines.append(f"- {r['outcome']} {r['test']}{revision}: {r.get('detail', '')[:200]}")
    lines.append("\n## Out of scope, by reason\n")
    for reason, n in collections.Counter(u.out for u in units if u.verdict == "out").most_common():
        lines.append(f"- {n} {reason}")
    lines.append("\n## Every unit that differs\n")
    for unit in scope:
        if unit.differences:
            lines.append(f"- {unit.label}: " + "; ".join(f"{k}: {d}" for k, d in unit.differences)[:400])
    return "\n".join(lines) + "\n", same, (sum(r["outcome"] == "match" for r in ran) if ran is not None else 0)


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    cfg = config(sys.argv[1])
    units = [u for test, path in collect(cfg.rust) for u in units_of(test, path)]
    todo = [u for u in units if not u.out]
    evaluate(os.path.abspath(cfg.bin), todo, cfg.jobs, os.path.join(cfg.work, "eval"))
    score(units)
    ran = run_pass(cfg) if cfg.run_pass else None
    text, same, matched = summary(units, ran)
    print(text)
    if cfg.out:
        os.makedirs(cfg.out, exist_ok=True)
        open(os.path.join(cfg.out, "summary.md"), "w").write(text)
        rows = [{"test": u.test, "revision": u.revision, "verdict": u.verdict, "out": u.out,
                 "expected": sorted(u.expected) if u.expected is not None else None,
                 "differences": u.differences} for u in units]
        json.dump({"units": rows, "run_pass": ran}, open(os.path.join(cfg.out, "units.json"), "w"), indent=1)
    panics = sum(u.verdict == "panic" for u in units)
    low = same < cfg.min_evaluation or (ran is not None and matched < cfg.min_run_pass)
    if panics or low:
        print(f"consteval_ui.py: {panics} files blaze panicked on; evaluation {same} (at least {cfg.min_evaluation}), "
              f"run-pass {matched} (at least {cfg.min_run_pass})", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
