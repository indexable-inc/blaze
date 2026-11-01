"""proof.py: everything that proves blaze v2 (docs/v2/plan/lanes/proof.md), run as one, into one report.

    uv run --frozen python harness/v2/proof.py '{"engine": {"kind": "v1", "blaze": "target/release/blaze"}, "out": "/tmp/proof"}'
    uv run --frozen python harness/v2/proof.py '{"engine": {...}, "out": "/tmp/proof",
        "checks": [{"name": "oracle", "given": {"rounds": 8}}, {"name": "budgets"}]}'
    uv run --frozen python harness/v2/proof.py selftest

The checks, each a script of this directory run with the same `engine` (v1 now, v2 later):

    oracle        oracle.py: the incremental build equals one from scratch after every edit
    corpus        corpus.py: real crates built, run and tested by blaze, judged by their own tests
    cases         frontend/cases.py: the front end's cases, each accepted or refused as its header says
    dbsp          dbsp/edits.py: edit sequences of dbsp built incrementally by blaze
    share         share.py: the instance-sharing programs print what Rust guarantees under blaze
    determinism   determinism.py: two builds on different core counts, byte for byte the same
    tightmemory   tightmemory.py: a build under a tight memory budget finishes, never OOMs
    macroeval     macroeval_corpus.py: blaze-macro-eval's corpus of proc macros
    budgets       budgets.py: the targets of docs/v2/plan/goals/targets.md

Each writes its result (report.py's shape) to `OUT/<check>.json` and what it printed to
`OUT/<check>.log`; this writes `OUT/report.json` (one outcome per check, the verdict of the
worst of its outcomes) and `OUT/report.md`: a line per check, the budgets as a table, and
every outcome that did not pass. A check that wrote no result is an error naming its exit
status and the end of its log. Exits 1 when any check failed or erred, or the engine is v2
and anything was unmeasured.

Input (JSON inline, a file's path, or `-`):
    engine        the blaze under test (engine.EngineSpec), given to every check
    out           the directory of the results
    work          a directory for the checks' work (`WORK/<check>`); each makes its own when empty
    checks        which to run, in order (default: all, as above); each a `name`, `given` (the
                  rest of that script's input, a JSON object: its docstring says the fields)
                  and `hours`, how long it may take before it is stopped
"""

import json
import subprocess
import sys
import time
import typing
from collections import Counter
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any, Literal

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]

import engine  # noqa: E402
import report  # noqa: E402
from typed_input import fail, parse  # noqa: E402

SCRIPT = "proof.py"
type Name = Literal["oracle", "corpus", "cases", "dbsp", "share", "determinism", "tightmemory", "macroeval",
                    "budgets"]
SCRIPTS: dict[Name, tuple[str, ...]] = {
    "oracle": ("oracle.py", "run"),
    "corpus": ("corpus.py",),
    "cases": ("frontend/cases.py",),
    "dbsp": ("dbsp/edits.py",),
    "share": ("share.py",),
    "determinism": ("determinism.py",),
    "tightmemory": ("tightmemory.py",),
    "macroeval": ("macroeval_corpus.py",),
    "budgets": ("budgets.py",),
}
WORST: tuple[report.Verdict, ...] = ("fail", "error", "unmeasured", "pass")


@dataclass(frozen=True)
class Check:
    name: Name
    given: Any = None
    hours: float = 12.0


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    out: str
    work: str = ""
    checks: tuple[Check, ...] = tuple(Check(name) for name in SCRIPTS)


def argv_of(check: Check, spec: engine.EngineSpec, out: Path, work: str) -> list[str]:
    raw: object = check.given
    if not isinstance(raw, dict | None):
        fail(SCRIPT, f"checks: {check.name}: `given` is a JSON object, not {json.dumps(raw)}")
    wanted = typing.cast("dict[str, object] | None", check.given)
    given: dict[str, object] = dict(wanted) if wanted else {}
    for field in ("engine", "out", "work"):
        if field in given:
            fail(SCRIPT, f"checks: {check.name}: `{field}` is proof.py's to give, not `given`'s")
    given.update(engine=asdict(spec), out=str(out / f"{check.name}.json"))
    if work:
        given["work"] = str(Path(work) / check.name)
    script, *sub = SCRIPTS[check.name]
    return [sys.executable, str(HERE / script), *sub, json.dumps(given)]


def verdict_of(outcomes: list[dict[str, Any]]) -> report.Verdict:
    seen = {o["verdict"] for o in outcomes}
    worst: list[report.Verdict] = [v for v in WORST if v in seen]
    return worst[0] if worst else "pass"


def run_check(check: Check, spec: engine.EngineSpec, out: Path, work: str) -> tuple[report.Verdict, str, dict[str, Any] | None]:
    argv = argv_of(check, spec, out, work)
    log = out / f"{check.name}.log"
    result_path = out / f"{check.name}.json"
    result_path.unlink(missing_ok=True)
    print(f"proof: {check.name}: {' '.join(argv[1:3])} (log {log})", flush=True)
    began = time.monotonic()
    with open(log, "w") as to:
        try:
            done = subprocess.run(argv, cwd=engine.ROOT, stdout=to, stderr=subprocess.STDOUT,
                                  stdin=subprocess.DEVNULL, timeout=check.hours * 3600)
            status = f"exit {done.returncode}"
        except subprocess.TimeoutExpired:
            status = f"stopped after {check.hours} h"
    took = time.monotonic() - began
    if not result_path.is_file():
        tail = log.read_text(errors="replace")[-3000:]
        return "error", f"{SCRIPTS[check.name][0]} wrote no result ({status}, {took:.0f} s):\n{tail}", None
    result: dict[str, Any] = json.loads(result_path.read_text())
    outcomes: list[dict[str, Any]] = result["outcomes"]
    counts = Counter(o["verdict"] for o in outcomes)
    verdict = verdict_of(outcomes) if outcomes else "error"
    if status.startswith("stopped"):
        verdict = "error"
    shown = ", ".join(f"{counts[v]} {v}" for v in WORST if counts[v])
    return verdict, f"{shown or 'no outcomes'} ({status}, {took:.0f} s)", result


def cell(value: object) -> str:
    return "" if value is None else str(value).replace("|", "\\|").replace("\n", " ")


def markdown(spec: engine.EngineSpec, rows: list[tuple[Check, report.Verdict, str, dict[str, Any] | None]]) -> str:
    lines = [
        f"# blaze v2 proof: {spec.kind}, {spec.blaze} ({spec.backend})",
        "",
        f"commit {report.commit()}, {report.now()}",
        "",
        "| check | verdict | outcomes |",
        "|---|---|---|",
    ]
    lines += [f"| {check.name} | {verdict} | {cell(detail.splitlines()[0])} |" for check, verdict, detail, _ in rows]
    budgets = next((result for check, _, _, result in rows if check.name == "budgets" and result), None)
    if budgets is not None:
        lines += ["", "## Budgets (docs/v2/plan/goals/targets.md)", "", "| budget | verdict | numbers | detail |",
                  "|---|---|---|---|"]
        for o in budgets["outcomes"]:
            numbers = "  ".join(f"{k}={v}" for k, v in o["numbers"].items() if v is not None)
            first = o["detail"].splitlines()[0] if o["detail"] else ""
            lines.append(f"| {cell(o['name'])} | {o['verdict']} | {cell(numbers)} | {cell(first)} |")
    for check, verdict, detail, result in rows:
        bad = [o for o in (result or {}).get("outcomes", []) if o["verdict"] != "pass"]
        if result is None:
            lines += ["", f"## {check.name}: {verdict}", "", "```", detail, "```"]
        elif bad:
            lines += ["", f"## {check.name}: what did not pass", ""]
            for o in bad[:200]:
                first = o["detail"].splitlines()[0] if o["detail"] else ""
                lines.append(f"- `{o['name']}` {o['verdict']}: {first[:300]}")
            if len(bad) > 200:
                lines.append(f"- and {len(bad) - 200} more, in {check.name}.json")
    return "\n".join(lines) + "\n"


def run(it: Input) -> None:
    if not it.checks:
        fail(SCRIPT, "checks: name at least one")
    names = [c.name for c in it.checks]
    if len(set(names)) != len(names):
        fail(SCRIPT, f"checks: each at most once, not {names}")
    out = Path(it.out).resolve()
    out.mkdir(parents=True, exist_ok=True)
    whole = report.Report(SCRIPT, it.engine, str(out / "report.json"))
    rows: list[tuple[Check, report.Verdict, str, dict[str, Any] | None]] = []
    for check in it.checks:
        verdict, detail, result = run_check(check, it.engine, out, it.work)
        rows.append((check, verdict, detail, result))
        counts: dict[str, report.Number] = dict((result or {}).get("counts", {}))
        whole.add(check.name, verdict, detail, **counts)
    whole.extra.update(results={c.name: str(out / f"{c.name}.json") for c, *_ in rows})
    (out / "report.md").write_text(markdown(it.engine, rows))
    print(f"proof: {out / 'report.md'}", flush=True)
    whole.finish()


def selftest(proof: Path | None) -> list[str]:
    """`proof` is the `blaze-v2-proof` binary that `engine.tree_digest` runs; without it, that
    check is one wrong line saying so."""
    wrong: list[str] = []
    if [verdict_of([{"verdict": v} for v in vs]) for vs in (["pass"], ["pass", "unmeasured"], ["error", "fail"], [])] != [
        "pass", "unmeasured", "fail", "pass"]:
        wrong.append("verdict_of does not give the worst verdict")
    spec = engine.EngineSpec("v1", "/b/blaze")
    argv = argv_of(Check("oracle", {"rounds": 2}), spec, Path("/o"), "/w")
    given = json.loads(argv[-1])
    if argv[1:3] != [str(HERE / "oracle.py"), "run"]:
        wrong.append(f"argv_of: {argv}")
    if given != {"rounds": 2, "engine": asdict(spec), "out": "/o/oracle.json", "work": "/w/oracle"}:
        wrong.append(f"argv_of gives {given}")
    if argv_of(Check("cases"), spec, Path("/o"), "")[1] != str(HERE / "frontend" / "cases.py"):
        wrong.append("argv_of does not find a check's script in a subdirectory")
    for name in SCRIPTS:
        if name not in typing.get_args(Name.__value__):
            wrong.append(f"{name} is a script but not a Name")
        if not (HERE / SCRIPTS[name][0]).is_file():
            wrong.append(f"{SCRIPTS[name][0]} is not in {HERE}")
    md = markdown(spec, [(Check("budgets"), "fail", "1 fail (exit 1, 3 s)", {"outcomes": [
        {"name": "nushell body", "verdict": "fail", "detail": "took 80 ms", "numbers": {"ms": 80, "budget": 30}}]})])
    if "| nushell body | fail | ms=80  budget=30 | took 80 ms |" not in md:
        wrong.append(f"markdown has no row of the budget:\n{md}")
    return wrong + v2_work_wrong() + files_wrong(proof) + v1_logs_wrong()


def old_outputs(target: Path, profile: str) -> dict[str, str]:
    """`engine.outputs` as it was (iterdir, an lstat a file, the whole file read and hashed on
    one core): its model."""
    import stat

    import blake3

    top = target / profile
    found: dict[str, str] = {}
    for sub in (top, top / "examples", top / "deps"):
        if not sub.is_dir():
            continue
        for path in sorted(sub.iterdir()):
            info = path.lstat()
            if not stat.S_ISREG(info.st_mode):
                continue
            if (bool(info.st_mode & 0o111) and not path.suffix) or path.suffix in engine.OUTPUT_SUFFIXES:
                found[str(path.relative_to(top))] = blake3.blake3(path.read_bytes()).hexdigest()
    return found


def old_first_difference(a: Path, b: Path) -> str:
    """`engine.first_difference` as it was (64 KiB reads, a byte loop): its model."""
    with open(a, "rb") as fa, open(b, "rb") as fb:
        at = 0
        while True:
            x, y = fa.read(1 << 16), fb.read(1 << 16)
            if x != y:
                for i, (p, q) in enumerate(zip(x, y)):
                    if p != q:
                        return f"byte {at + i:#x}: {p:#04x} against {q:#04x}"
                return f"one ends at byte {at + min(len(x), len(y)):#x}"
            if not x:
                return "the same"
            at += len(x)


def old_tree_digest(root: Path, skip: tuple[str, ...]) -> str:
    """`engine.tree_digest` as it was (`os.walk`, an `lstat` an entry, the files hashed in
    Python): its model, whose digests runs before the change kept."""
    import os
    import stat

    import blake3

    files: list[tuple[str, Path, bool]] = []
    links: list[tuple[str, str]] = []
    for here, dirs, names in os.walk(root):
        base = Path(here)
        if base == root:
            dirs[:] = [d for d in dirs if d not in skip]
        for name in [*dirs, *names]:
            path = base / name
            info = path.lstat()
            rel = path.relative_to(root).as_posix()
            if stat.S_ISLNK(info.st_mode):
                links.append((rel, os.readlink(path)))
            elif stat.S_ISREG(info.st_mode):
                files.append((rel, path, bool(info.st_mode & 0o111)))
    files.sort()
    links.sort()
    h = blake3.blake3(b"blaze v2 proof tree 2\0")
    for rel, path, executable in files:
        h.update(f"f\0{rel}\0{int(executable)}\0{blake3.blake3(path.read_bytes()).hexdigest()}\n".encode())
    for rel, to in links:
        h.update(f"l\0{rel}\0{to}\n".encode())
    return h.hexdigest()


def files_wrong(proof: Path | None) -> list[str]:
    """`engine.outputs`, `first_difference` and `tree_digest` against their models, on
    files made here: sizes across the megabyte buffers, every kind of entry `outputs` skips."""
    import os
    import random
    import tempfile

    wrong: list[str] = []
    rng = random.Random(1606)
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        top = root / "target" / "debug"
        for sub in (top / "deps", top / "examples", top / ".fingerprint"):
            sub.mkdir(parents=True)
        names = ["app", "libx.rlib", "libx.rmeta", "x.d", "x.o", "libz.so", "test-1a2b", "empty.rlib", ".cargo-lock"]
        for i, name in enumerate(names):
            for sub in (top, top / "deps", top / "examples"):
                size = rng.choice([0, 1, 4095, (1 << 20) + 3, 3 << 20]) if i % 3 == 0 else rng.randrange(2000)
                (sub / name).write_bytes(rng.randbytes(size))
                if name in ("app", "test-1a2b", "x.o"):
                    (sub / name).chmod(0o755)
        (top / "deps" / "link").symlink_to(top / "app")
        (top / "deps" / "dir.rlib").mkdir()
        if (got := engine.outputs(root / "target")) != (want := old_outputs(root / "target", "debug")):
            wrong.append(f"engine.outputs: {sorted(set(got.items()) ^ set(want.items()))[:4]}")
        if engine.outputs(root / "target", "release"):
            wrong.append("engine.outputs: a profile with no directory has outputs")
        a, b = root / "a", root / "b"
        for size in (0, 1, 64, 65, 1 << 16, (1 << 20) - 1, 1 << 20, (1 << 20) + 1, (2 << 20) + 77):
            body = rng.randbytes(size)
            a.write_bytes(body)
            cases = [body, body + b"x", body[:-1] if body else b"y", body[: size // 2]]
            for at in {0, size // 3, size - 1, (1 << 20) - 1, 1 << 20} if size else set[int]():
                if 0 <= at < size:
                    other = bytearray(body)
                    other[at] ^= 0x10
                    if at + 7 < size:
                        other[-1] ^= 1
                    cases.append(bytes(other))
            for other in cases:
                b.write_bytes(other)
                for first, second in ((a, b), (b, a)):
                    if (got_d := engine.first_difference(first, second)) != (want_d := old_first_difference(first, second)):
                        wrong.append(f"engine.first_difference, {size} bytes: {got_d!r}, not {want_d!r}")
                    if proof is not None and (by := engine.first_difference_by(proof, first, second)) != want_d:
                        wrong.append(f"engine.first_difference_by, {size} bytes: {by!r}, not {want_d!r}")
        src = root / "src"
        (src / "target").mkdir(parents=True)
        (src / "lib.rs").write_text("fn f() {}\n")
        (src / "target" / "out").write_text("built\n")
        (src / "build.sh").write_text("#!/bin/sh\n")
        (src / "deep" / "er").mkdir(parents=True)
        for n in range(150):
            (src / "deep" / "er" / f"m{n}.rs").write_text(f"fn m{n}() {{}}\n")
        (src / "big.bin").write_bytes(rng.randbytes((1 << 20) + 5))

        def tree(when: str) -> str:
            if proof is None:
                raise engine.Failed("no `blaze-v2-proof` given: `proof.py selftest PATH`")
            got = engine.tree_digest(src, ("target",), proof)
            if got != (want := old_tree_digest(src, ("target",))):
                wrong.append(f"engine.tree_digest {when}: {got}, not the model's {want}")
            return got

        try:
            before = tree("at first")
            (src / "target" / "out").write_text("built again\n")
            if tree("after a skipped write") != before:
                wrong.append("engine.tree_digest reads a directory it skips")
            for change in ("edit", "mode", "link", "rename"):
                if change == "edit":
                    (src / "lib.rs").write_text("fn g() {}\n")
                elif change == "mode":
                    (src / "build.sh").chmod(0o755)
                elif change == "link":
                    os.symlink("lib.rs", src / "alias.rs")
                else:
                    (src / "lib.rs").rename(src / "main.rs")
                after = tree(f"after a {change}")
                if after == before:
                    wrong.append(f"engine.tree_digest misses a {change}")
                before = after
        except engine.Failed as e:
            wrong.append(f"engine.tree_digest: {e}")
    return wrong


def v1_logs_wrong() -> list[str]:
    """`V1.work_since` reads only what each worker's log gained after the mark, and all of it."""
    import tempfile

    wrong: list[str] = []
    with tempfile.TemporaryDirectory() as tmp:
        home = Path(tmp)
        logs = [home / "workers" / str(i) / "log" for i in (1, 2)]
        for log in logs:
            log.parent.mkdir(parents=True)
            log.write_text("stats: 100 of 100 functions compiled\n")
        eng = engine.V1(engine.EngineSpec("v1", sys.executable))
        mark = eng.mark(home)
        with open(logs[0], "a") as f:
            f.write("stats: 3 of 10 functions compiled\nlink: app (spliced into the running program)\n")
        with open(logs[1], "a") as f:
            f.write("store: 5 functions found, 2 compiled and added\nstats: 2 of 9 functions compiled\n")
        work = eng.work_since(home, mark)
        if (work.compiled, work.found, work.link) != (5, 5, "spliced"):
            wrong.append(f"V1.work_since: {work}")
    return wrong


def v2_work_wrong() -> list[str]:
    """`engine.V2` reading what requests did from the `blaze-work` lines of canned output, the
    form `blaze_ui::api::work_json` prints."""
    line = ('{"reason": "blaze-work", "reran": %d, "green": 300, "compiled": %d, "found": 40, '
            '"link": "%s", "critical_path_us": %d, "links": %s}')
    spliced = '[{"path": "target/debug/app", "how": "spliced", "why": []}]'
    full = '[{"path": "target/debug/app", "how": "full", "why": ["exports", "debug"]}]'
    first = '[{"path": "target/debug/new", "how": "first", "why": []}]'

    class Canned(engine.V2):
        def __init__(self, said: list[str]):
            super().__init__(engine.EngineSpec("v2", sys.executable))
            self.said = said

        def command(self, step: str, command: str, args: list[str], ws: Path, env: dict[str, str],
                    timeout: float = 7200) -> engine.Ran:
            return engine.Ran((command, *args), 0.1, 0, self.said.pop(0), "")

    wrong: list[str] = []
    home, ws = Path("/h"), Path("/w")
    env = {"BLAZE_HOME": str(home)}
    json_ = ["--message-format=json"]
    eng = Canned(['{"reason": "compiler-artifact"}\n' + line % (12, 2, "patched", 1500, spliced) + "\n",
                  line % (3, 1, "placed", 2_500_000, full) + "\n", "test a ... ok\n" + line % (1, 0, "patched", 9, spliced) + "\n"])
    eng.build("cold", ws, env, json_)
    mark = eng.mark(home)
    eng.build("edit", ws, env, json_)
    eng.build("tests", ws, env, json_, command="test")
    work = eng.work_since(home, mark)
    if (work.compiled, work.reran, work.found, work.green) != (1, 4, 80, 600):
        wrong.append(f"V2.work_since sums {work}")
    if work.link != "placed whole: edit (`blaze build --message-format=json`): target/debug/app (exports, debug)":
        wrong.append(f"V2.work_since: link {work.link!r}")
    # A first link of a new image is not a fallback; a line without `links` is refused.
    for links, want in ((first, "first link: edit (`blaze build --message-format=json`): target/debug/new"),
                        (spliced, "patched in place")):
        other = Canned([line % (1, 1, "placed", 1, links) + "\n"])
        other.build("edit", ws, env, json_)
        if (got := other.work_since(home, engine.Mark({"requests": 0})).link) != want:
            wrong.append(f"V2.work_since: link {got!r}, wanted {want!r}")
    other = Canned([line.replace(', "links": %s', "") % (1, 1, "patched", 1) + "\n"])
    other.build("edit", ws, env, json_)
    try:
        other.work_since(home, engine.Mark({"requests": 0}))
        wrong.append("V2.work_since read a `blaze-work` line without `links`")
    except engine.Failed:
        pass
    if eng.critical_path_s(ws, env) != 9e-6:
        wrong.append(f"V2.critical_path_s: {eng.critical_path_s(ws, env)}")
    for args, said in ((json_, "test a ... ok\n"), ([], line % (1, 1, "patched", 1, spliced))):
        mark = eng.mark(home)
        eng.said.append(said)
        eng.build("unsaid", ws, env, args, command="test")
        try:
            eng.work_since(home, mark)
            wrong.append(f"V2.work_since counted a request with args {args} that said {said!r}")
        except engine.Failed as e:
            if "unsaid (`blaze test" not in str(e) or "no `blaze-work` line" not in str(e):
                wrong.append(f"V2.work_since: {e}")
        try:
            eng.critical_path_s(ws, env)
            wrong.append("V2.critical_path_s read a request that printed no `blaze-work` line")
        except engine.Failed:
            pass
    bad = [line % (1, 1, "spliced", 1, spliced), line.replace('"found": 40', '"found": -1') % (1, 1, "patched", 1, spliced)]
    for said in bad:
        try:
            engine.required("bad", engine.worked_lines(said)[0])
            wrong.append(f"engine.required took {said}")
        except engine.Failed:
            pass
    if len(engine.worked_lines(line % (1, 1, "patched", 1, spliced) + "\n{not json\n")) != 1:
        wrong.append("engine.worked_lines does not skip a line that is not JSON")
    return wrong


def main() -> None:
    match sys.argv[1:]:
        case ["selftest", *rest] if len(rest) <= 1:
            wrong = selftest(Path(rest[0]) if rest else None)
            for line in wrong:
                print(line)
            print(f"{SCRIPT} selftest: {len(wrong)} wrong")
            sys.exit(1 if wrong else 0)
        case [text]:
            run(parse(Input, text, SCRIPT))
        case _:
            fail(SCRIPT, f"usage: {SCRIPT} JSON, or selftest (see the top of the file)")


if __name__ == "__main__":
    main()
