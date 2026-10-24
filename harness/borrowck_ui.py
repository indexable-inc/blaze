#!/usr/bin/env python3
"""Scores ra-jit's borrow checker against rustc's own borrow-check UI tests.

usage: borrowck_ui.py <rust checkout> [--bin ra-jit] [--work dir] [--jobs N] [--filter substr]
                      [--json out.json] [--toolchain nightly-YYYY-MM-DD] [--mode polonius|nll|both]
                      [--list causes|failing|out] [--min-pass N]

The tests are those of `tests/ui/borrowck`, `tests/ui/nll` (with `nll/polonius`), every
`tests/ui` file with a revision that turns Polonius on, and every other `tests/ui` file whose
expected output has an error a borrow checker reports (E0382, E0499, E0502 and the rest of
`BORROWCK`). Each revision of a test is a unit.

What a unit should give is what the nightly rustc of the checkout's commit gives, run on the
file as compiletest would run it: once as `-Zpolonius=next` (the primary score, Polonius
alpha) and once without (NLL, the secondary score). The errors rustc reports are checked
against the test's own `//~ ERROR` annotations for the revision's own mode, and the units
where the two disagree are counted, so a harness mistake shows.

A unit passes when the borrow-check errors ra-jit reports and the ones rustc reports are the
same set of (error code, line); an error rustc gives no code ("lifetime may not live long
enough") is named by its message (`CODELESS`). Message text is recorded, not scored.

A unit is out of scope, with a counted reason, when it cannot say anything about the borrow
checker here: it needs a nightly feature, an auxiliary crate or another file, a `-Z` flag
other than Polonius, rustc stops before borrow checking, or rust-analyzer reports an error in
it that rustc does not. The list of reasons is the worklist for making the scope larger.
"""
import argparse, collections, concurrent.futures, hashlib, json, os, re, shlex, subprocess, sys, tempfile, time

# Errors only a borrow checker reports.
BORROWCK = {
    "E0373", "E0381", "E0382", "E0384", "E0499", "E0500", "E0501", "E0502", "E0503", "E0504", "E0505",
    "E0506", "E0507", "E0508", "E0509", "E0510", "E0515", "E0521", "E0524", "E0594", "E0596", "E0597",
    "E0626", "E0712", "E0713", "E0716", "E0309", "E0310", "E0311", "E0621", "E0700", "E0792", "E0793",
}
# Errors rustc reports without a code, named by what their message starts with.
CODELESS = {
    "lifetime may not live long enough": "lifetime",
    "captured variable cannot escape `FnMut` closure body": "fnmut-escape",
    "higher-ranked lifetime error": "higher-ranked",
    "higher-ranked subtype error": "higher-ranked",
    "variable does not need to be mutable": "unused_mut",
    "returns a reference to a captured variable which escapes the closure body": "fnmut-escape",
    "returns a value referencing data owned by the current function": "E0515",
}
NOT_GENERAL = re.compile(r"^implementation of `.*` is not general enough$")
# rustc's error for a type that must outlive every lifetime of a `for<'a>`, which names the type.
FORALL_OUTLIVES = re.compile(r"^(`.+`|the parameter type) does not live long enough$")
# Flags that do not change what the borrow checker says.
INERT = re.compile(r"^(-[AWDF] ?\S*|--cap-lints(=| )\S+|-g|-C ?\S+|-O|--crate-type(=| )\S+|--check-cfg(=| )\S+"
                   r"|--error-format(=| )\S+|--diagnostic-width(=| )\S+|-Z ?(ui-testing|deduplicate-diagnostics|"
                   r"track-diagnostics|treat-err-as-bug|polonius|next-solver|verbose-internals|write-long-types-to-disk|"
                   r"translate-remapped-path-to-local-path|validate-mir)(=\S+)?|--crate-name(=| )\S+|--test|--cfg(=| )\S+"
                   r"|--emit(=| )\S+|--emit"
                   r"|--remap-path-prefix(=| )\S+|--edition(=| )\S+|-Z ?unpretty=\S+|-Z ?dump-mir\S*)$")
LINE = r"^//@ *(?:\[(?P<rev>[\w,-]+)\] *)?"
DIRECTIVE = re.compile(LINE + r"(?P<name>[\w-]+)(?: *: *| +|$)(?P<value>.*)$", re.M)
ANNOTATION = re.compile(r"//(?:\[(?P<revs>[\w,-]+)\])?~(?P<adjust>\||\^+|v+)? *(?P<kind>ERROR|WARN|WARNING|NOTE|HELP|SUGGESTION)\b(?P<text>.*)$")
FEATURE = re.compile(r"#!\[(cfg_attr\(.*, *)?feature\(")
MODULE = re.compile(r"^\s*(?:#\[path *= *\"[^\"]+\"\]\s*)?(?:pub(?:\([\w: ]+\))? )?mod \w+;", re.M)
INCLUDE = re.compile(r"\binclude(?:_str|_bytes)?! *\(")
DEFAULT_TOOLCHAIN = "nightly-2026-10-09"


def key_of(code, message):
    """What an error is scored by: its code, or for a codeless one the name of its message."""
    if code:
        return code
    for start, name in CODELESS.items():
        if message.startswith(start):
            return name
    if NOT_GENERAL.match(message):
        return "not-general"
    if FORALL_OUTLIVES.match(message):
        return "forall-outlives"
    return None


def is_borrowck(key):
    return key is not None and (key in BORROWCK or key in CODELESS.values()
                                or key in ("not-general", "forall-outlives"))


class Unit:
    def __init__(self, test, path, revision):
        self.test, self.path, self.revision = test, path, revision
        self.edition, self.flags, self.mode = "2015", [], "nll"
        self.out = None  # why the unit says nothing about the borrow checker here
        self.crate_type = None
        self.sibling_polonius = False

    @property
    def label(self):
        return f"{self.test}#{self.revision}" if self.revision else self.test


def flags_of(text):
    words, flags = shlex.split(text), []
    while words:
        word = words.pop(0)
        takes = word in ("-C", "-Z", "--cfg", "--check-cfg", "--edition", "--crate-type", "--cap-lints", "-A", "-W",
                         "-D", "-F", "--crate-name", "--extern", "-L", "-l", "--emit", "--target", "--error-format")
        flags.append(f"{word} {words.pop(0)}" if takes and words else word)
    return flags


def polonius_of(flags):
    for flag in flags:
        flag = flag.replace(" ", "")
        if flag.startswith("-Zpolonius"):
            value = flag.partition("=")[2]
            off = {"off", "no", "n", "false"}
            return "polonius" if value == "next" else "nll" if value in off else "legacy"
    return None


def collect(root):
    """The test files: the borrow-check directories, Polonius revisions, borrow-check stderr."""
    ui = os.path.join(root, "tests", "ui")
    found = set()
    code = re.compile(r"^error\[(E\d{4})\]", re.M)
    codeless = re.compile(r"^error: (lifetime may not live long enough|captured variable cannot escape)", re.M)
    for dirpath, dirnames, filenames in os.walk(ui):
        dirnames[:] = [d for d in dirnames if d != "auxiliary"]
        rel = os.path.relpath(dirpath, ui)
        whole = rel.split(os.sep)[0] in ("borrowck", "nll")
        stems = set()
        for name in filenames:
            if name.endswith(".stderr"):
                text = open(os.path.join(dirpath, name), errors="replace").read()
                if any(c in BORROWCK for c in code.findall(text)) or codeless.search(text):
                    stems.add(name.split(".")[0])
        for name in filenames:
            if not name.endswith(".rs"):
                continue
            path = os.path.join(dirpath, name)
            if whole or name[:-3] in stems:
                found.add(path)
            elif "polonius" in open(path, errors="replace").read(2000):
                text = open(path, errors="replace").read()
                if re.search(r"^//@ *revisions:.*polonius", text, re.M):
                    found.add(path)
    return sorted(found)


def classify(root, path):
    """The units of a test file, each with its edition, flags and mode, or why it is out."""
    test = os.path.relpath(path, os.path.join(root, "tests", "ui"))
    text = open(path, errors="replace").read()
    directives = list(DIRECTIVE.finditer(text))
    revisions = [r for d in directives if d["name"] == "revisions" for r in d["value"].split()] or [None]
    units = []
    for revision in revisions:
        unit = Unit(test, path, revision)
        if revision:
            unit.flags.append(f"--cfg {revision}")
        for d in directives:
            revs = d["rev"].split(",") if d["rev"] else None
            if revs is not None and revision not in revs:
                continue
            name, value = d["name"], d["value"].strip()
            if name == "edition":
                year = re.match(r"\d+", value)
                if not year:
                    unit.out = f"edition `{value}` (unstable)"
                else:
                    unit.edition = year.group(0)
            elif name == "compile-flags":
                try:
                    unit.flags += flags_of(value)
                except ValueError:
                    unit.out = unit.out or "compile flags that do not split"
            elif name in ("aux-build", "aux-crate", "proc-macro", "aux-bin", "aux-codegen-backend"):
                unit.out = unit.out or "needs an auxiliary crate"
        mode = polonius_of(unit.flags)
        unit.mode = mode or "nll"
        unit.flags = [f for f in unit.flags if not f.replace(" ", "").startswith("-Zpolonius")]
        for flag in unit.flags:
            if flag.startswith("--edition"):
                unit.edition = re.split(r"[= ]", flag, maxsplit=1)[1]
            if flag.startswith("--crate-type"):
                unit.crate_type = re.split(r"[= ]", flag, maxsplit=1)[1]
        if unit.out is None and FEATURE.search(text):
            unit.out = "needs a nightly feature (`#![feature]`)"
        if unit.out is None and (MODULE.search(text) or INCLUDE.search(text)):
            unit.out = "reads another source file (`mod x;`, `include!`)"
        if unit.out is None and mode == "legacy":
            unit.out = "revision for `-Zpolonius=legacy` (the datalog analysis, not the target)"
        if unit.out is None:
            odd = [f for f in unit.flags if not INERT.match(f)]
            if odd:
                unit.out = f"compile flag {odd[0].split('=')[0]}"
        if unit.out is None and unit.edition not in ("2015", "2018", "2021", "2024"):
            unit.out = f"edition {unit.edition}"
        m = re.search(r'#!\[crate_type *= *"(\w+)"\]', text)
        if m and not unit.crate_type:
            unit.crate_type = m.group(1)
        units.append(unit)
    if any(u.mode == "polonius" for u in units):
        for u in units:
            u.sibling_polonius = True
    return units


def annotations(text, revision):
    """The lines that the test says have an error, in the given revision."""
    lines, last = set(), None
    for number, line in enumerate(text.split("\n"), 1):
        m = ANNOTATION.search(line)
        if not m:
            continue
        revs = m["revs"].split(",") if m["revs"] else None
        adjust = m["adjust"] or ""
        if adjust == "|":
            target = last
        elif adjust.startswith("^"):
            target = number - len(adjust)
        elif adjust.startswith("v"):
            target = number + len(adjust)
        else:
            target = number
        last = target
        if revs is not None and revision not in revs:
            continue
        if m["kind"] == "ERROR" and target:
            lines.add(target)
    return lines


def rewrite(unit, text, all_revisions):
    """The file as rustc sees it in the unit's revision: `cfg(rev)` true for its own revision
    and false for the others, lines kept where they are."""
    for rev in all_revisions:
        if rev is None:
            continue
        on = "all()" if rev == unit.revision else "any()"
        text = re.sub(rf"\bcfg\(\s*{re.escape(rev)}\s*\)", f"cfg({on})", text)
        text = re.sub(rf"(cfg(?:_attr)?\((?:not|any|all)\([^)]*)\b{re.escape(rev)}\b", rf"\1{on}", text)
    return text


def rustc_errors(toolchain, unit, mode, cache, work):
    """The errors rustc reports for the unit in `mode`, each (key, line, message), the file's
    own only; or a string saying why rustc gave none."""
    text = open(unit.path, errors="replace").read()
    crate_type = unit.crate_type or "bin"
    args = [unit.path, "--edition", unit.edition, "--error-format=json", "--emit=metadata", "--crate-type", crate_type,
            "-Zui-testing", "-A", "unused", "-A", "internal_features"]
    for flag in unit.flags:
        if flag.startswith(("--crate-type", "--edition", "--error-format", "--emit", "-Zpolonius", "-Z polonius")):
            continue
        args += flag.split(" ", 1)
    # Nightly builds turn Polonius alpha on by default (`CFG_DEFAULT_POLONIUS_NEXT`), so both
    # modes are named.
    args.append("-Zpolonius=next" if mode == "polonius" else "-Zpolonius=off")
    key = hashlib.sha1(json.dumps([toolchain, args, text]).encode()).hexdigest()
    if key not in cache:
        with tempfile.TemporaryDirectory(dir=work) as out:
            try:
                done = subprocess.run(["rustc", f"+{toolchain}", *args, "--out-dir", out], capture_output=True,
                                      text=True, timeout=120, cwd=os.path.dirname(unit.path))
                cache[key] = [l for l in done.stderr.splitlines() if '"level":"error' in l]
            except subprocess.TimeoutExpired:
                cache[key] = "rustc timed out"
    if isinstance(cache[key], str):
        return cache[key]
    errors = []
    for line in cache[key]:
        try:
            d = json.loads(line)
        except ValueError:
            continue
        if d.get("$message_type") != "diagnostic" or d.get("level") not in ("error", "error: internal compiler error"):
            continue
        if d.get("level") != "error":
            return "rustc crashed"
        message = d["message"]
        if message.startswith("aborting due to"):
            continue
        code = (d.get("code") or {}).get("code")
        primary = [s for s in d.get("spans", []) if s.get("is_primary")]
        line = None
        for span in primary:
            # An error in what a macro expanded to is on the line of the outermost call in the file.
            while span and os.path.basename(span["file_name"]) != os.path.basename(unit.path):
                span = (span.get("expansion") or {}).get("span")
            if span:
                line = span["line_start"]
                break
        errors.append((key_of(code, message) or f"other: {code or message[:60]}", line, message))
    return errors


def ra_jit_run(binary, units, mode, jobs, work):
    """What ra-jit reports for each unit: {label: {"diagnostics": [...]} or {"panic": ...}}."""
    by_edition = collections.defaultdict(list)
    files = {}
    for unit in units:
        text = open(unit.path, errors="replace").read()
        revisions = [r for d in DIRECTIVE.finditer(text) if d["name"] == "revisions" for r in d["value"].split()]
        copy = os.path.join(work, "src", hashlib.sha1(unit.label.encode()).hexdigest()[:16] + ".rs")
        os.makedirs(os.path.dirname(copy), exist_ok=True)
        with open(copy, "w") as f:
            f.write(rewrite(unit, text, revisions))
        files[copy] = unit.label
        cfgs = [f[6:].strip() for f in unit.flags if f.startswith(("--cfg ", "--cfg="))]
        by_edition[unit.edition].append(copy + ("\t" + " ".join(cfgs) if cfgs else ""))
    env = dict(os.environ, RA_JIT_BORROWCK=mode)
    env["RUST_BACKTRACE"] = "0"
    results = {}

    def run(edition, chunk):
        proc = subprocess.run([binary, "borrowck", "--edition", edition], input="\n".join(chunk) + "\n",
                              capture_output=True, text=True, env=env, timeout=3600)
        got = {}
        for line in proc.stdout.splitlines():
            try:
                d = json.loads(line)
            except ValueError:
                continue
            got[files[d["path"]]] = d
        for line in chunk:
            path = line.split("\t", 1)[0]
            if files[path] not in got:
                got[files[path]] = {"panic": f"ra-jit exited ({proc.returncode}): {proc.stderr[-300:]}"}
        return got

    work_items = []
    for edition, paths in by_edition.items():
        share = max(1, (len(paths) + jobs - 1) // jobs)
        for i in range(0, len(paths), share):
            work_items.append((edition, paths[i:i + share]))
    with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as pool:
        for got in pool.map(lambda it: run(*it), work_items):
            results.update(got)
    return results


def score(units, expected, actual, mode):
    """Each unit's verdict: ("pass"|"fail"|"out", reason or detail)."""
    verdicts = {}
    for unit in units:
        if unit.out:
            verdicts[unit.label] = ("out", unit.out, None)
            continue
        want = expected[unit.label]
        if isinstance(want, str):
            verdicts[unit.label] = ("out", want, None)
            continue
        got = actual.get(unit.label, {})
        if "panic" in got:
            verdicts[unit.label] = ("fail", "ra-jit panicked: " + got["panic"].splitlines()[0][:120], None)
            continue
        diags = got.get("diagnostics", [])
        rustc_keys = {k for k, _, _ in want}
        want_b = {(k, l) for k, l, _ in want if is_borrowck(k)}
        other = [(k, l) for k, l, _ in want if not is_borrowck(k)]
        if other and not want_b:
            verdicts[unit.label] = ("out", "rustc stops before borrow checking", None)
            continue
        for d in diags:
            # An error rustc gives no code rust-analyzer reports as `borrowck`.
            d["code"] = key_of(None if d["code"] == "borrowck" else d["code"], d["message"]) or d["code"]
        # rustc's errors, which a lint is where it is denied.
        got_b = {(d["code"], d["line"]) for d in diags if is_borrowck(d["code"]) and d["severity"] == "Error"}
        stray = sorted({d["code"] for d in diags if d["severity"] == "Error" and not is_borrowck(d["code"])
                        and d["code"] not in rustc_keys})
        if stray:
            verdicts[unit.label] = ("out", f"rust-analyzer reports {stray[0]}, rustc does not", None)
            continue
        skipped = [d for d in diags if d["code"] == "borrowck-skipped"]
        detail = {"want": sorted(want_b, key=str), "got": sorted(got_b, key=str),
                  "messages": [m for k, _, m in want if is_borrowck(k)], "skipped": [d["message"] for d in skipped]}
        if want_b == got_b:
            verdicts[unit.label] = ("pass", "", detail)
        else:
            missing, extra = want_b - got_b, got_b - want_b
            if extra and not want_b:
                cause = "false positive: " + ", ".join(sorted({k for k, _ in extra}))
            elif missing and not got_b:
                cause = "not reported: " + ", ".join(sorted({k for k, _ in missing}))
            else:
                cause = "differs: " + ", ".join(sorted({k for k, _ in missing ^ extra}))
            if skipped:
                cause += " (body skipped)"
            verdicts[unit.label] = ("fail", cause, detail)
    return verdicts


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("rust")
    ap.add_argument("--bin", default=os.environ.get("RA_JIT_BIN", "ra-jit"))
    ap.add_argument("--work", default=os.path.join(tempfile.gettempdir(), "borrowck-ui"))
    ap.add_argument("--jobs", type=int, default=os.cpu_count())
    ap.add_argument("--filter")
    ap.add_argument("--json")
    ap.add_argument("--toolchain", default=DEFAULT_TOOLCHAIN)
    ap.add_argument("--mode", default="both", choices=["polonius", "nll", "both"])
    ap.add_argument("--list", choices=["causes", "failing", "out", "passing"], action="append", default=[])
    ap.add_argument("--min-pass", type=int, help="exit 1 if fewer in-scope Polonius units pass")
    ap.add_argument("--no-ra-jit", action="store_true", help="only collect and check rustc's verdicts")
    args = ap.parse_args()
    os.makedirs(args.work, exist_ok=True)
    cache_path = os.path.join(args.work, "rustc-cache.json")
    cache = json.load(open(cache_path)) if os.path.exists(cache_path) else {}

    files = collect(args.rust)
    if args.filter:
        files = [f for f in files if args.filter in f]
    units = [u for f in files for u in classify(args.rust, f)]
    modes = ["polonius", "nll"] if args.mode == "both" else [args.mode]
    # A revision that only turns Polonius off says in NLL mode what its sibling says in Polonius mode.
    in_mode = {
        "polonius": [u for u in units if not (u.sibling_polonius and u.mode == "nll")],
        "nll": [u for u in units if not (u.sibling_polonius and u.mode == "polonius")],
    }
    started = time.time()
    expected = {m: {} for m in modes}
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        for mode in modes:
            todo = [u for u in in_mode[mode] if not u.out]
            for unit, errs in zip(todo, pool.map(lambda u: rustc_errors(args.toolchain, u, mode, cache, args.work), todo)):
                expected[mode][unit.label] = errs
    json.dump(cache, open(cache_path, "w"))
    print(f"rustc: {sum(len(v) for v in expected.values())} runs in {time.time() - started:.0f}s", file=sys.stderr)

    # rustc against the test's own annotations, in the revision's own mode.
    agree = disagree = 0
    disagreeing = []
    natural = os.environ.get("BORROWCK_UI_NATURAL", "polonius")
    for unit in units:
        mode = unit.mode if unit.sibling_polonius else natural
        if unit.out or mode not in expected or unit.label not in expected[mode]:
            continue
        errs = expected[mode][unit.label]
        if isinstance(errs, str):
            continue
        text = open(unit.path, errors="replace").read()
        said = annotations(text, unit.revision)
        if re.search(r"^//@ *(\[[\w,-]+\])? *dont-require-annotations", text, re.M):
            said_ok = {l for _, l, _ in errs if l} >= said
        else:
            said_ok = {l for _, l, _ in errs if l} == said
        if said_ok:
            agree += 1
        else:
            disagree += 1
            disagreeing.append(unit.label)

    report = {"toolchain": args.toolchain, "rust": args.rust, "files": len(files), "units": len(units),
              "annotations_agree": agree, "annotations_disagree": disagreeing, "modes": {}}
    if args.no_ra_jit:
        print(f"rustc agrees with the annotations on {agree} units, not on {disagree}")
        for label in disagreeing:
            print(f"  {label}")
        return
    exit_code = 0
    for mode in modes:
        todo = [u for u in in_mode[mode] if not u.out and not isinstance(expected[mode].get(u.label), str)]
        started = time.time()
        actual = ra_jit_run(args.bin, todo, mode, args.jobs, args.work)
        took = time.time() - started
        verdicts = score(in_mode[mode], expected[mode], actual, mode)
        count = collections.Counter(v[0] for v in verdicts.values())
        inscope = count["pass"] + count["fail"]
        out = collections.Counter(v[1] for v in verdicts.values() if v[0] == "out")
        causes = collections.Counter(v[1] for v in verdicts.values() if v[0] == "fail")
        accept_agree = sum(1 for l, v in verdicts.items() if v[0] != "out" and v[2] is not None
                           and bool(v[2]["want"]) == bool(v[2]["got"]))
        skipped_pass = sum(1 for v in verdicts.values() if v[0] == "pass" and v[2] and v[2]["skipped"])
        print(f"\n== {mode}: {count['pass']} of {inscope} in-scope units pass "
              f"({100 * count['pass'] / max(inscope, 1):.1f}%), {count['out']} out of scope; ra-jit took {took:.0f}s")
        print(f"   accept/reject agrees on {accept_agree} of {inscope}; {skipped_pass} passing units had a body skipped")
        print("   out of scope:")
        for why, n in out.most_common():
            print(f"     {n:5} {why}")
        if "causes" in args.list or True:
            print("   failing, by cause:")
            for why, n in causes.most_common(40):
                print(f"     {n:5} {why}")
        for kind, name in (("failing", "fail"), ("out", "out"), ("passing", "pass")):
            if kind in args.list:
                print(f"   {kind}:")
                for label, v in sorted(verdicts.items()):
                    if v[0] == name:
                        print(f"     {label}: {v[1]} {json.dumps(v[2]) if v[2] and name == 'fail' else ''}")
        report["modes"][mode] = {
            "pass": count["pass"], "fail": count["fail"], "out": count["out"], "accept_agree": accept_agree,
            "out_reasons": dict(out), "fail_causes": dict(causes),
            "verdicts": {l: list(v) for l, v in verdicts.items()},
        }
        if args.min_pass is not None and mode == "polonius" and count["pass"] < args.min_pass:
            print(f"FAIL: {count['pass']} in-scope Polonius units pass, fewer than {args.min_pass}")
            exit_code = 1
        # A unit ra-jit panics on has none of its errors reported, and an error on a unit
        # rustc accepts is a false one: either fails the run, whatever the passes.
        panicked = sorted(l for l, v in verdicts.items() if v[0] == "fail" and v[1].startswith("ra-jit panicked"))
        false_errors = sorted(l for l, v in verdicts.items() if v[0] == "fail" and v[2] and not v[2]["want"])
        for what, labels in (("ra-jit panicked on", panicked), ("errors rustc does not report on", false_errors)):
            if labels:
                print(f"FAIL ({mode}): {what} {len(labels)} units")
                for label in labels:
                    print(f"     {label}: {verdicts[label][1]}")
                exit_code = 1
    print(f"\nrustc agrees with the tests' annotations on {agree} units, not on {disagree}")
    if args.json:
        json.dump(report, open(args.json, "w"), indent=1)
    sys.exit(exit_code)


if __name__ == "__main__":
    main()
