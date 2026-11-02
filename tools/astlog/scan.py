"""The astlog lint gate: every rules file in `rules` over every file under `paths`, one file at a time.

    PYTHONPATH=python/typed-input/src python3 tools/astlog/scan.py \
        '{"astlog": "target/debug/astlog", "rules": "tools/astlog/rules", "paths": ["crates", "tools"]}'

The files are git's (tracked, and untracked ones git does not ignore) with an extension of a
language the rules file matches. Each is scanned on its own: astlog joins by nested loops over
the whole corpus, so a join over one file stays small where one over the tree would not, and
blaze's rules all look within a file. Prints the findings, `file:line:column: severity[rule]:
message`, and exits 1 when any is an error. `exclude` names paths (files or directories) left
unscanned; `jobs` is how many scans run at once (0: the CPUs). With `"fix": true` it applies
the rewrites of every rules file that has any instead (`astlog fix --write`, file by file), and
prints the files it changed.
"""

import json
import os
import re
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path

from typed_input import parse

SCRIPT = "scan.py"
# The extensions of each language astlog parses (tools/astlog/langs), by the name a rule matches it by.
EXTENSIONS = {
    "rust": (".rs",),
    "rs": (".rs",),
    "toml": (".toml",),
    "python": (".py", ".pyi"),
    "py": (".py", ".pyi"),
}


@dataclass(frozen=True)
class Input:
    astlog: str
    rules: str
    paths: tuple[str, ...]
    exclude: tuple[str, ...] = ()
    jobs: int = 0
    fix: bool = False


@dataclass(frozen=True)
class Finding:
    file: str
    line: int
    column: int
    severity: str
    rule: str
    message: str


def languages(rules: Path) -> set[str]:
    """The languages a rules file's `(match <lang> ...)` atoms name, lowercased."""
    return {m.lower() for m in re.findall(r"\(match\s+([A-Za-z+#]+)", rules.read_text())}


def files(paths: tuple[str, ...], exclude: tuple[str, ...]) -> list[str]:
    run = subprocess.run(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", *paths],
        capture_output=True,
        check=False,
    )
    if run.returncode != 0:
        sys.exit(
            f"scan.py: listing {' '.join(paths)} needs {os.getcwd()} to be a git work tree: "
            f"git ls-files exited {run.returncode}: {run.stderr.decode().strip()}"
        )
    listed = run.stdout.decode()
    skip = tuple(e.rstrip("/") for e in exclude)
    return sorted(
        {
            f
            for f in listed.split("\0")
            if f and os.path.isfile(f) and not any(f == s or f.startswith(s + "/") for s in skip)
        }
    )


def scan(astlog: str, rules: Path, file: str) -> list[Finding]:
    done = subprocess.run(
        [astlog, "scan", "--json", str(rules), file], capture_output=True, text=True, check=False
    )
    try:
        found = json.loads(done.stdout)
    except json.JSONDecodeError:
        sys.exit(
            f"{SCRIPT}: `astlog scan {rules} {file}` exited {done.returncode} without findings:\n"
            f"{done.stdout}{done.stderr}"
        )
    return [Finding(f["file"], f["line"], f["column"], f["severity"], f["rule"], f["message"]) for f in found]


def fix(astlog: str, rules: Path, file: str) -> list[str]:
    before = Path(file).read_bytes()
    done = subprocess.run(
        [astlog, "fix", str(rules), file, "--write"], capture_output=True, text=True, check=False
    )
    if done.returncode != 0:
        sys.exit(f"{SCRIPT}: `astlog fix {rules} {file} --write` exited {done.returncode}:\n{done.stderr}")
    return [file] if Path(file).read_bytes() != before else []


def main() -> None:
    match sys.argv[1:]:
        case [text]:
            given = parse(Input, text, SCRIPT)
        case _:
            print(
                f'usage: {SCRIPT} \'{{"astlog": "<binary>", "rules": "<dir>", "paths": [...]}}\'',
                file=sys.stderr,
            )
            sys.exit(2)
    rules = sorted(Path(given.rules).glob("*.astlog"))
    if not rules:
        sys.exit(f"{SCRIPT}: no .astlog file in {given.rules}")
    corpus = files(given.paths, given.exclude)
    jobs = given.jobs or os.cpu_count() or 1
    work: list[tuple[Path, str]] = []
    if given.fix:
        rules = [r for r in rules if "(rewrite" in r.read_text()]
    for rule in rules:
        suffixes = tuple(s for lang in languages(rule) for s in EXTENSIONS.get(lang, ()))
        if not suffixes:
            sys.exit(
                f"{SCRIPT}: {rule} matches no language whose files this script knows: "
                f"{sorted(languages(rule))}"
            )
        work.extend((rule, f) for f in corpus if f.endswith(suffixes))

    if given.fix:

        def rewrite(job: tuple[Path, str]) -> list[str]:
            return fix(given.astlog, *job)

        # One rules file at a time: two rules files' rewrites of one file must not race.
        changed: set[str] = set()
        for rule in rules:
            mine = [job for job in work if job[0] == rule]
            with ThreadPoolExecutor(jobs) as pool:
                changed.update(f for batch in pool.map(rewrite, mine) for f in batch)
        for f in sorted(changed):
            print(f)
        print(f"{SCRIPT}: {len(work)} fixes of {len(corpus)} files: {len(changed)} changed", file=sys.stderr)
        return

    def run(job: tuple[Path, str]) -> list[Finding]:
        return scan(given.astlog, *job)

    with ThreadPoolExecutor(jobs) as pool:
        found = sorted(
            (f for batch in pool.map(run, work) for f in batch),
            key=lambda f: (f.file, f.line, f.column, f.rule),
        )
    for f in found:
        print(f"{f.file}:{f.line}:{f.column}: {f.severity}[{f.rule}]: {f.message}")
    errors = sum(f.severity == "error" for f in found)
    print(
        f"{SCRIPT}: {len(work)} scans of {len(corpus)} files by {len(rules)} rules files: "
        f"{errors} errors, {len(found) - errors} warnings",
        file=sys.stderr,
    )
    sys.exit(1 if errors else 0)


if __name__ == "__main__":
    main()
