"""std_uses.py: the worklist of moving blaze's crates onto its own standard library (bstd,
docs/v2/plan/design/std.md, step S3), and the check of ledger row S3: every use of Rust's std
that bstd replaces, per crate, in the order to port in.

    uv run --frozen python harness/v2/std_uses.py '{"engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/std-uses.json"}'
    uv run --frozen python harness/v2/std_uses.py '{"engine": {...}, "out": "...", "table": "/tmp/std-uses.md", "crates": ["blaze-ids"]}'
    uv run --frozen python harness/v2/std_uses.py '{"command": "fix", "engine": {...}, "out": "..."}'
    uv run --frozen python harness/v2/std_uses.py '{"command": "read", "messages": "/tmp/check.json", "table": "/tmp/std-uses.md"}'
    uv run --frozen python harness/v2/std_uses.py selftest

It runs `blaze check --message-format=json` over the workspace with the lint `blaze::std_use`
(docs/lints.md#std_use) at `warn` (`RUSTFLAGS=-W blaze::std_use`, which comes after the
workspace's own lint tables, so it holds whatever they and the lint's switch say today),
reads the lint's findings from the JSON messages, and writes one table per crate: its findings
by kind and by item, how many carry a machine-applicable fix, and an estimate of the port.

The input is one JSON object (inline, a file's path, or `-`):

    command   report  run the check and write the worklist (the default)
              fix     apply the machine-applicable fixes crate by crate, in the worklist's
                      order: `blaze check --fix --lint blaze::std_use -p CRATE`, then
                      `blaze check -p CRATE`; stops at the first crate whose check then fails,
                      naming it, and leaves the crates before it fixed
              read    no blaze: read the JSON messages a `blaze check` wrote (`messages`) and
                      write the worklist from them
    engine    the blaze that checks (engine.EngineSpec); `report` and `fix` need one
    out       the result's path (report.py's JSON); empty for none
    root      the checkout whose workspace is checked (".")
    crates    only these packages (`-p` each); empty for the whole workspace
    table     where the worklist is written as Markdown; empty to print it
    messages  `read`: the file of `--message-format=json` lines
    attribute also hold every crate to having `#![blaze::capabilities]` in its root file, the
              other half of S3's objective (false)
    minutes   the most one `blaze check` may take (120)
    work      the daemon's home for the run; empty for a new directory under /tmp

The outcomes (report.py): `std-uses` passes when the lint has no finding left, with the counts
by kind as its numbers; `capabilities-attribute`, with `attribute`, when no crate lacks the
attribute; `fix` names how many crates were fixed and the one that stopped it. A check that
fails for another reason (the workspace does not check) is an error naming what blaze said,
never a pass with nothing found. Exit status 1 unless every outcome passes.

The worklist. A crate's row has its findings (one per item per function, as the lint reports
them), the uses they count, how many findings have a fix, and how many functions have a
finding with none: each of those is a function whose signature changes (it takes a capability,
a `Cx` or an allocator), which is what the port costs. Its size is `small` up to 10 such
functions, `medium` up to 60, `large` beyond. Crates are sorted by layer, then by size: layer
0 has no workspace dependency with findings (a leaf of the port), layer N depends on one of
layer N-1. A layer's crates do not depend on each other's findings, so a layer is a batch.

What a finding is is read from its first line, whose shape blaze-lints' `std_use::message`
writes and its test holds:

    `ITEM` (KIND), used N times in `fn NAME`, WHY: WHAT INSTEAD
    `ITEM` (KIND), declared here, WHY: WHAT INSTEAD
"""

import json
import re
import sys
import tomllib
from collections import Counter
from dataclasses import dataclass, field
from pathlib import Path
from typing import Literal, cast

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
import engine
import report
from engine import EngineSpec
from typed_input import fail, parse

SCRIPT = "std_uses.py"
LINT = "blaze::std_use"
# The lint's kinds, as its findings name them (blaze-lints' `std_use::Kind::say`).
KINDS = ("ambient authority", "hidden cost", "global-heap allocation", "renamed")
FIRST_LINE = re.compile(
    r"^`(?P<item>[^`]+)` \((?P<kind>[^)]+)\), "
    r"(?:used (?P<count>once|\d+ times) in (?P<where>.+?)|declared here), "
)
ATTRIBUTE = re.compile(r"^\s*#!\[\s*blaze\s*::\s*capabilities\s*\]", re.M)
SMALL, MEDIUM = 10, 60


@dataclass(frozen=True)
class Input:
    command: Literal["report", "fix", "read"] = "report"
    engine: EngineSpec | None = None
    out: str = ""
    root: str = "."
    crates: tuple[str, ...] = ()
    table: str = ""
    messages: str = ""
    attribute: bool = False
    minutes: int = 120
    work: str = ""


@dataclass(frozen=True)
class Finding:
    """One finding of the lint: an item of std one function uses, or a declaration."""

    crate: str
    file: str
    line: int
    column: int
    item: str
    kind: str
    uses: int
    where: str
    fix: bool


@dataclass(frozen=True)
class Package:
    """A package of the workspace: its directory and the workspace packages it depends on
    (`[dependencies]` and `[build-dependencies]`; a dev-dependency is not ported first)."""

    name: str
    dir: Path
    deps: tuple[str, ...] = ()


@dataclass(frozen=True)
class Crate:
    """A crate's row of the worklist."""

    name: str
    layer: int
    findings: tuple[Finding, ...]
    by_kind: dict[str, int] = field(default_factory=dict[str, int])
    by_item: dict[str, int] = field(default_factory=dict[str, int])

    @property
    def uses(self) -> int:
        return sum(f.uses for f in self.findings)

    @property
    def fixes(self) -> int:
        return sum(f.fix for f in self.findings)

    @property
    def functions(self) -> int:
        """The functions with a finding that has no fix: each one's signature changes."""
        return len({(f.file, f.where or f"{f.line}:{f.column}") for f in self.findings if not f.fix})

    @property
    def size(self) -> str:
        return "small" if self.functions <= SMALL else "medium" if self.functions <= MEDIUM else "large"


def table_of(path: Path) -> dict[str, object]:
    try:
        return tomllib.loads(path.read_text())
    except (OSError, tomllib.TOMLDecodeError) as e:
        fail(SCRIPT, f"{path}: {e}")


def as_table(value: object) -> dict[str, object]:
    return cast(dict[str, object], value) if isinstance(value, dict) else {}


def packages(root: Path) -> dict[str, Package]:
    """The workspace's packages by name, from its manifests: the root's `members` (a `dir/*`
    glob or a directory) less its `exclude`, and each member's dependencies on other members."""
    workspace = as_table(table_of(root / "Cargo.toml").get("workspace"))
    excluded = {str(e) for e in cast(list[object], workspace.get("exclude") or [])}
    dirs: list[Path] = []
    for member in cast(list[object], workspace.get("members") or []):
        found = sorted(root.glob(str(member))) if "*" in str(member) else [root / str(member)]
        dirs += [d for d in found if (d / "Cargo.toml").is_file() and d.relative_to(root).as_posix() not in excluded]
    # A workspace dependency's key is not always its package's name (`package = "..."`).
    renamed = {key: str(as_table(spec).get("package", key))
               for key, spec in as_table(workspace.get("dependencies")).items()}
    read: list[tuple[str, Path, dict[str, object]]] = []
    for d in dirs:
        manifest = table_of(d / "Cargo.toml")
        name = as_table(manifest.get("package")).get("name")
        if isinstance(name, str):
            read.append((name, d, manifest))
    names = {name for name, _, _ in read}
    out: dict[str, Package] = {}
    for name, d, manifest in read:
        deps: set[str] = set()
        tables = [manifest, *as_table(manifest.get("target")).values()]
        for holder in tables:
            for section in ("dependencies", "build-dependencies"):
                for key, spec in as_table(as_table(holder).get(section)).items():
                    own = as_table(spec)
                    dep = str(own.get("package", renamed.get(key, key) if own.get("workspace") else key))
                    if dep in names and dep != name:
                        deps.add(dep)
        out[name] = Package(name, d, tuple(sorted(deps)))
    return out


def package_of(message: dict[str, object], by_dir: dict[str, str]) -> str:
    """The package a compiler message is of: by its manifest's directory, else by the name in
    its package id (`path+file:///w/crates/a#0.1.0`, `path+file:///w#a@0.1.0`, `a 0.1.0 (..)`)."""
    manifest = message.get("manifest_path")
    if isinstance(manifest, str) and (found := by_dir.get(str(Path(manifest).parent))):
        return found
    spec = str(message.get("package_id", ""))
    if "#" in spec:
        front, _, back = spec.partition("#")
        return back.split("@")[0] if "@" in back else front.rstrip("/").rsplit("/", 1)[-1]
    return spec.split(" ")[0]


def has_fix(diagnostic: dict[str, object]) -> bool:
    """Whether the diagnostic, or a child of it, suggests a machine-applicable replacement."""
    spans = cast(list[dict[str, object]], diagnostic.get("spans") or [])
    if any(s.get("suggestion_applicability") == "MachineApplicable" for s in spans):
        return True
    return any(has_fix(c) for c in cast(list[dict[str, object]], diagnostic.get("children") or []))


def findings(lines: str, pkgs: dict[str, Package]) -> tuple[list[Finding], list[str]]:
    """The lint's findings in a `--message-format=json` stream, each once (a crate's lib and
    its lib as a test both give it), and the first lines this script could not read: a finding
    whose shape changed is said, never dropped."""
    by_dir = {str(p.dir): p.name for p in pkgs.values()}
    found: dict[tuple[str, str, int, int, str], Finding] = {}
    unread: list[str] = []
    for line in lines.splitlines():
        if not line.startswith("{"):
            continue
        try:
            message = cast(dict[str, object], json.loads(line))
        except json.JSONDecodeError:
            continue
        if message.get("reason") != "compiler-message":
            continue
        diagnostic = as_table(message.get("message"))
        if as_table(diagnostic.get("code")).get("code") != LINT:
            continue
        text = str(diagnostic.get("message", ""))
        read = FIRST_LINE.match(text)
        if read is None or read["kind"] not in KINDS:
            unread.append(text)
            continue
        spans = cast(list[dict[str, object]], diagnostic.get("spans") or [])
        primary = next((s for s in spans if s.get("is_primary")), spans[0] if spans else {})
        count = read["count"]
        uses = 1 if count in (None, "once") else int(count.split()[0])
        crate = package_of(message, by_dir)
        at = (str(primary.get("file_name", "")), cast(int, primary.get("line_start", 0)),
              cast(int, primary.get("column_start", 0)))
        found.setdefault((crate, *at, read["item"]), Finding(
            crate, *at, read["item"], read["kind"], uses, read["where"] or "", has_fix(diagnostic)))
    return sorted(found.values(), key=lambda f: (f.crate, f.file, f.line, f.column, f.item)), unread


def layers(pkgs: dict[str, Package], with_findings: set[str]) -> dict[str, int]:
    """Each package's layer: 0 with no dependency, direct or through others, that has findings;
    else one more than the deepest such dependency's. A package the manifests do not have (the
    stream named it) is layer 0."""
    known: dict[str, int] = {}
    visiting: set[str] = set()

    def beneath(name: str) -> int:
        """The most layers of crates with findings under `name`, itself not counted."""
        if name in known:
            return known[name]
        if name in visiting or name not in pkgs:
            return 0
        visiting.add(name)
        deepest = max((beneath(d) + (d in with_findings) for d in pkgs[name].deps), default=0)
        visiting.discard(name)
        known[name] = deepest
        return deepest

    return {name: beneath(name) for name in sorted(set(pkgs) | with_findings)}


def worklist(found: list[Finding], pkgs: dict[str, Package]) -> list[Crate]:
    """The crates with findings, leaves first: by layer, then the fewest functions to change."""
    names = {f.crate for f in found}
    layer = layers(pkgs, names)
    crates: list[Crate] = []
    for name in names:
        own = tuple(f for f in found if f.crate == name)
        by_kind = Counter(f.kind for f in own)
        by_item = Counter[str]()
        for f in own:
            by_item[f.item] += f.uses
        crates.append(Crate(name, layer.get(name, 0), own, {k: by_kind[k] for k in KINDS if by_kind[k]},
                            dict(sorted(by_item.items(), key=lambda it: (-it[1], it[0])))))
    return sorted(crates, key=lambda c: (c.layer, c.functions, c.name))


def markdown(crates: list[Crate], clean: int) -> str:
    total = sum(len(c.findings) for c in crates)
    lines = [
        "# std uses bstd replaces: the port's worklist", "",
        f"{total} findings of `{LINT}` in {len(crates)} crates; {clean} crates have none. A layer's crates "
        "depend on no findings of each other: port a layer as a batch, layer 0 first.", "",
        "| order | crate | layer | size | functions to change | findings | uses | with a fix | "
        + " | ".join(KINDS) + " |",
        "|---|---|---|---|---|---|---|---|" + "---|" * len(KINDS),
    ]
    for n, c in enumerate(crates, 1):
        kinds = " | ".join(str(c.by_kind.get(k, 0)) for k in KINDS)
        lines.append(f"| {n} | {c.name} | {c.layer} | {c.size} | {c.functions} | {len(c.findings)} | {c.uses} | "
                     f"{c.fixes} | {kinds} |")
    for c in crates:
        lines += ["", f"## {c.name}", "",
                  f"Layer {c.layer}, {c.size}: {c.functions} functions to change, {len(c.findings)} findings, "
                  f"{c.uses} uses, {c.fixes} with a fix.", "", "| item | kind | uses | functions | fix |",
                  "|---|---|---|---|---|"]
        for item, uses in c.by_item.items():
            own = [f for f in c.findings if f.item == item]
            lines.append(f"| `{item}` | {own[0].kind} | {uses} | {len(own)} | {'yes' if own[0].fix else 'no'} |")
    return "\n".join(lines) + "\n"


def without_attribute(pkgs: dict[str, Package]) -> list[str]:
    """The packages whose root file (`[lib] path`, else `src/lib.rs`, else `src/main.rs`) does
    not open with `#![blaze::capabilities]`."""
    lacking: list[str] = []
    for p in pkgs.values():
        lib = as_table(table_of(p.dir / "Cargo.toml").get("lib")).get("path")
        roots = [p.dir / str(lib)] if isinstance(lib, str) else [p.dir / "src/lib.rs", p.dir / "src/main.rs"]
        root = next((r for r in roots if r.is_file()), None)
        if root is None or not ATTRIBUTE.search(root.read_text(errors="replace")):
            lacking.append(p.name)
    return sorted(lacking)


def selection(crates: tuple[str, ...], pkgs: dict[str, Package]) -> list[str]:
    if unknown := sorted(set(crates) - set(pkgs)):
        fail(SCRIPT, f"crates: the workspace has no package {', '.join(unknown)}")
    return [arg for c in crates for arg in ("-p", c)] or ["--workspace"]


def errors_of(ran: engine.Ran) -> list[str]:
    """The first lines of the errors a check's JSON stream has."""
    out: list[str] = []
    for line in ran.stdout.splitlines():
        if line.startswith("{") and '"compiler-message"' in line:
            try:
                diagnostic = as_table(cast(dict[str, object], json.loads(line)).get("message"))
            except json.JSONDecodeError:
                continue
            if diagnostic.get("level") == "error":
                out.append(str(diagnostic.get("message", "")))
    return out


def judge(rep: report.Report, crates: list[Crate], clean: int, unread: list[str], failed: str) -> None:
    """The outcome `std-uses`: an error when the check did not finish or a finding was not
    read, else pass exactly when no finding is left."""
    numbers: dict[str, report.Number] = {
        "findings": sum(len(c.findings) for c in crates), "crates": len(crates), "clean": clean,
        "uses": sum(c.uses for c in crates), "with_fix": sum(c.fixes for c in crates),
        "functions": sum(c.functions for c in crates),
    }
    numbers |= {k.replace(" ", "_").replace("-", "_"): sum(c.by_kind.get(k, 0) for c in crates) for k in KINDS}
    if failed:
        rep.add("std-uses", "error", failed, **numbers)
    elif unread:
        rep.add("std-uses", "error", f"{len(unread)} findings of {LINT} this script cannot read; the first: "
                f"{unread[0][:200]}", **numbers)
    elif crates:
        first = ", ".join(f"{c.name} ({len(c.findings)})" for c in crates[:6])
        rep.add("std-uses", "fail", f"{numbers['findings']} findings in {len(crates)} crates; port first: {first}",
                **numbers)
    else:
        rep.add("std-uses", "pass", "", **numbers)


def main(it: Input) -> None:
    root = Path(it.root).resolve()
    pkgs = packages(root)
    chosen = selection(it.crates, pkgs)
    spec = it.engine or EngineSpec("v2", "")
    rep = report.Report(SCRIPT, spec, it.out)
    failed = ""
    eng: engine.Engine | None = None
    env: dict[str, str] = {}
    if it.command == "read":
        if not it.messages:
            fail(SCRIPT, "read needs `messages`, the file of `blaze check --message-format=json` lines")
        stream = Path(it.messages).read_text()
    else:
        if it.engine is None:
            fail(SCRIPT, f"{it.command} needs `engine`, the blaze that checks")
        try:
            eng = engine.Engine.of(it.engine)
            # `scrubbed` drops the caller's RUSTFLAGS; the lint's flag is the run's own.
            env = eng.env(engine.work_dir(it.work, "std-uses-") / "home", extra={"RUSTFLAGS": f"-W {LINT}"})
            ran = eng.build("the check", root, env, [*chosen, "--message-format=json"], "check", it.minutes * 60)
        except engine.Failed as e:
            fail(SCRIPT, str(e))
        stream = ran.stdout
        if ran.returncode != 0:
            said = errors_of(ran)
            failed = (f"`blaze check` exited {ran.returncode} with {len(said)} errors, so the findings are not all "
                      f"of the workspace; the first: {(said[0] if said else ran.said(600))[:600]}")
    found, unread = findings(stream, pkgs)
    crates = worklist(found, pkgs)
    scope = set(it.crates) or set(pkgs)
    page = markdown(crates, len(scope - {c.name for c in crates}))
    if it.table:
        Path(it.table).write_text(page)
    else:
        print(page)
    rep.extra["worklist"] = [
        {"crate": c.name, "layer": c.layer, "size": c.size, "functions": c.functions, "findings": len(c.findings),
         "uses": c.uses, "with_fix": c.fixes, "by_kind": c.by_kind, "by_item": c.by_item} for c in crates]
    judge(rep, crates, len(scope - {c.name for c in crates}), unread, failed)
    if it.attribute:
        lacking = [name for name in without_attribute(pkgs) if name in scope]
        rep.add("capabilities-attribute", "fail" if lacking else "pass",
                f"{len(lacking)} crates without `#![blaze::capabilities]`; the first: {', '.join(lacking[:8])}"
                if lacking else "", lacking=len(lacking), crates=len(scope))
    if eng is not None:
        try:
            if it.command == "fix" and not failed:
                fix(rep, eng, root, env, [c for c in crates if c.fixes], it.minutes * 60)
            eng.stop(root, env)
        except engine.Failed as e:
            rep.add("fix" if it.command == "fix" else "std-uses", "error", str(e))
    rep.finish()


def fix(rep: report.Report, eng: engine.Engine, root: Path, env: dict[str, str], crates: list[Crate],
        timeout: float) -> None:
    """Applies the fixes crate by crate in the worklist's order, checking each crate after its
    fix; stops at the first whose check fails, naming it."""
    done: list[str] = []
    for c in crates:
        fixed = eng.build(f"fixing {c.name}", root, env, ["--fix", "--lint", LINT, "-p", c.name], "check", timeout)
        checked = fixed if fixed.returncode != 0 else eng.build(
            f"checking {c.name} fixed", root, env, ["-p", c.name, "--message-format=json"], "check", timeout)
        if checked.returncode != 0:
            said = errors_of(checked)
            rep.add("fix", "fail", f"stopped at {c.name}: `{' '.join(checked.argv[1:])}` exited "
                    f"{checked.returncode}: {(said[0] if said else checked.said(600))[:600]}; fixed before it: "
                    f"{', '.join(done) or 'none'}", fixed=len(done), stopped_at=c.name)
            return
        done.append(c.name)
    rep.add("fix", "pass", f"fixed: {', '.join(done) or 'no crate has a finding with a fix'}", fixed=len(done))


def expect(holds: bool, what: str) -> None:
    if not holds:
        raise SystemExit(f"{SCRIPT} selftest: {what}")


def message(manifest: str, text: str, file: str, line: int, applicability: str | None = None,
            code: str = LINT, level: str = "warning") -> str:
    span = {"file_name": file, "line_start": line, "column_start": 5, "is_primary": True,
            "suggestion_applicability": None}
    children = [{"message": "write it by bstd's name", "spans": [span | {"suggestion_applicability": applicability}],
                 "children": []}] if applicability else []
    return json.dumps({"reason": "compiler-message", "package_id": f"path+file://{Path(manifest).parent}#0.1.0",
                       "manifest_path": manifest,
                       "message": {"message": text, "code": {"code": code}, "level": level, "spans": [span],
                                   "children": children}})


def selftest() -> None:
    import tempfile
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        (root / "Cargo.toml").write_text(
            '[workspace]\nmembers = ["crates/*", "tools/t"]\nexclude = ["crates/out"]\n'
            '[workspace.dependencies]\nleaf = { path = "crates/leaf" }\nnamed = { path = "crates/mid", package = "mid" }\n')
        manifests = {
            "crates/leaf": '[package]\nname = "leaf"\n',
            "crates/pure": '[package]\nname = "pure"\n[dependencies]\nleaf.workspace = true\n',
            "crates/mid": '[package]\nname = "mid"\n[dependencies]\npure = { path = "../pure" }\nserde = "1"\n',
            "crates/top": '[package]\nname = "top"\n[lib]\npath = "lib.rs"\n[build-dependencies]\nnamed.workspace = true\n'
                          '[dev-dependencies]\ntool = { path = "../../tools/t" }\n',
            "crates/out": '[package]\nname = "out"\n',
            "tools/t": '[package]\nname = "tool"\n[target.\'cfg(unix)\'.dependencies]\ntop = { path = "../../crates/top" }\n',
        }
        for d, text in manifests.items():
            (root / d / "src").mkdir(parents=True)
            (root / d / "Cargo.toml").write_text(text)
            (root / d / "src/lib.rs").write_text("//! a crate\n")
        (root / "crates/top/lib.rs").write_text("//! top\n#![blaze::capabilities]\n")
        pkgs = packages(root)
        expect(sorted(pkgs) == ["leaf", "mid", "pure", "tool", "top"], f"members less the excluded: {sorted(pkgs)}")
        expect(pkgs["mid"].deps == ("pure",) and pkgs["pure"].deps == ("leaf",), "a path and a workspace dependency")
        expect(pkgs["top"].deps == ("mid",), f"a renamed workspace dependency, and no dev-dependency: {pkgs['top'].deps}")
        expect(pkgs["tool"].deps == ("top",), "a target's dependencies count")
        expect(without_attribute(pkgs) == ["leaf", "mid", "pure", "tool"], f"who lacks the attribute: {without_attribute(pkgs)}")

        def at(crate: str) -> str:
            return str(root / "crates" / crate / "Cargo.toml")

        read = "`std::fs::read` (ambient authority), used 3 times in `fn load`, takes the file system from nowhere: take a `Dir`"
        stream = "\n".join([
            "   Compiling noise",
            message(at("leaf"), read, "crates/leaf/src/lib.rs", 4),
            message(at("leaf"), read, "crates/leaf/src/lib.rs", 4),  # the lib as a test gives it again
            message(at("leaf"), "`std::sync::Arc` (renamed), used once in `fn share`, has one name in bstd, and this "
                    "is not it: write `bstd::sync::Shared`", "crates/leaf/src/lib.rs", 9, "MachineApplicable"),
            message(at("leaf"), "`static mut` (ambient authority), declared here, is global mutable state, out of "
                    "every signature: a value someone owns", "crates/leaf/src/lib.rs", 12),
            message(at("top"), "`std::fs::write` (hidden cost), used once in `fn save`, writes a file with no word: "
                    "`dir.write(..)`", "crates/top/lib.rs", 2),
            message(at("top"), "`std::fs::read` (ambient authority), used once in `fn save`, takes the file system "
                    "from nowhere: take a `Dir`", "crates/top/lib.rs", 3, "MaybeIncorrect"),
            message(at("mid"), "unused variable: `x`", "crates/mid/src/lib.rs", 1, code="unused_variables"),
            message(at("mid"), "a finding of another shape", "crates/mid/src/lib.rs", 2),
            json.dumps({"reason": "build-finished", "success": True}),
        ])
        found, unread = findings(stream, pkgs)
        expect(unread == ["a finding of another shape"], f"a finding it cannot read is said: {unread}")
        expect(len(found) == 5, f"each finding once, other lints left out: {len(found)}")
        first = found[0]
        expect((first.crate, first.item, first.kind, first.uses, first.where, first.fix)
               == ("leaf", "std::fs::read", "ambient authority", 3, "`fn load`", False), f"a finding read: {first}")
        expect([f.fix for f in found if f.crate == "leaf"] == [False, True, False], "only a machine-applicable fix counts")
        expect(not any(f.fix for f in found if f.crate == "top"), "a fix that may be wrong is none")
        declared = next(f for f in found if f.item == "static mut")
        expect(declared.uses == 1 and declared.where == "", "a declaration is one use of no function")
        crates = worklist(found, pkgs)
        expect([(c.name, c.layer) for c in crates] == [("leaf", 0), ("top", 1)],
               f"leaves first, and a layer counts findings through crates with none: {[(c.name, c.layer) for c in crates]}")
        leaf, top = crates
        expect((len(leaf.findings), leaf.uses, leaf.fixes, leaf.functions) == (3, 5, 1, 2), "leaf's row")
        expect(top.functions == 1 and top.size == "small", "two findings of one function are one function to change")
        expect(leaf.by_kind == {"ambient authority": 2, "renamed": 1}, f"by kind: {leaf.by_kind}")
        expect(list(leaf.by_item.items())[0] == ("std::fs::read", 3), "items by uses, most first")
        expect(layers(pkgs, {"leaf", "pure", "top"})["top"] == 2, "a chain of crates with findings is as deep as it is long")
        cyclic = {"a": Package("a", root, ("b",)), "b": Package("b", root, ("a",))}
        expect(layers(cyclic, {"a", "b"})["a"] <= 2, "a cycle ends")
        page = markdown(crates, 3)
        expect("| 1 | leaf | 0 | small | 2 | 3 | 5 | 1 | 2 | 0 | 0 | 1 |" in page, f"leaf's line:\n{page}")
        expect("| `std::sync::Arc` | renamed | 1 | 1 | yes |" in page and "5 findings of `blaze::std_use` in 2 crates; 3 crates have none" in page, "the page")
        expect(package_of({"package_id": "path+file:///w#a@0.1.0"}, {}) == "a"
               and package_of({"package_id": "path+file:///w/crates/b#0.1.0"}, {}) == "b"
               and package_of({"package_id": "c 0.1.0 (path+file:///w/c)"}, {}) == "c", "a package id's name")
        spec = EngineSpec("v2", "blaze")

        def verdict(crates: list[Crate], unread: list[str], failed: str) -> tuple[str, str]:
            rep = report.Report(SCRIPT, spec, "")
            judge(rep, crates, 0, unread, failed)
            return rep.outcomes[0].verdict, rep.outcomes[0].detail

        expect(verdict([], [], "")[0] == "pass", "no finding left is the row met")
        failing = verdict(crates, [], "")
        expect(failing[0] == "fail" and "port first: leaf (3), top (2)" in failing[1], f"findings fail: {failing}")
        expect(verdict([], ["x"], "")[0] == "error", "a finding not read is no pass")
        expect(verdict([], [], "`blaze check` exited 101")[0] == "error", "a check that did not finish is no pass")
        ran = engine.Ran(("blaze", "check"), 1.0, 101, message(at("mid"), "mismatched types", "a.rs", 1, level="error"), "")
        expect(errors_of(ran) == ["mismatched types"], "a check's errors are read from its stream")
        expect(selection((), pkgs) == ["--workspace"] and selection(("leaf", "top"), pkgs) == ["-p", "leaf", "-p", "top"], "what is checked")
        given = parse(Input, '{"engine": {"kind": "v2", "blaze": "b"}, "out": "o", "attribute": true}', SCRIPT)
        expect(given.command == "report" and given.attribute and given.minutes == 120, "the ledger's input reads")
    print(f"{SCRIPT} selftest: ok")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest\n\n{__doc__}")
    if sys.argv[1] == "selftest":
        selftest()
    else:
        main(parse(Input, sys.argv[1], SCRIPT))
