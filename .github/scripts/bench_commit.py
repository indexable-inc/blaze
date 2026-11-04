#!/usr/bin/env python3
"""bench_commit.py: commit what a benchmark workflow wrote to main, with the trend charts drawn again.

    bench_commit.py '{"message": "bench: numbers for 1234567890 [skip ci]", "paths": ["README.md", "docs/benchmarks/"],
                      "site": true}'

The one way bench-readme.yaml, bench-counts.yaml and live-latency.yaml commit their results
(docs/benchmarks.md, "When it runs and what it commits"). It stages `paths`, commits them as
github-actions[bot] (nothing staged, nothing to do) and pushes to main with the checkout's token,
which must be one the ruleset on main lets past (BLAZE_BOT_TOKEN, docs/harness.md "CI"). Main
may have moved during the run, so each of up to `attempts` tries fetches it, rebases the commit
onto it and draws the trends again (`harness/benchcharts.py`) from the history as it is then,
amending the commit with them, before it pushes. Those workflows run at the same time and each
redraws every trend, so their commits can conflict in the trend charts and README's and
docs/benchmarks.md's trend sections: a conflict only in those files is settled by drawing them
again from the merged history; a conflict anywhere else stops with the files named. `site`
starts site.yaml on main once the push lands.

The JSON is inline, a file's path, or `-` for stdin. Needs git, and gh with GH_TOKEN for `site`.
Python 3.11+ (the runner's python3), stdlib.
"""

import asyncio
import json
import sys
from dataclasses import dataclass, fields
from pathlib import Path
from typing import cast

# What benchcharts.py's trends write: the files a conflict may be in and be drawn again.
CHARTS = "docs/benchmarks/charts/"
DRAWN = ("README.md", "docs/benchmarks.md")
BOT = ("github-actions[bot]", "41898282+github-actions[bot]@users.noreply.github.com")
MARKERS = ("<<<<<<< ", "=======", ">>>>>>> ")


@dataclass(frozen=True)
class Input:
    message: str
    paths: tuple[str, ...]
    site: bool = False
    attempts: int = 5


def typed(text: str) -> Input:
    """An Input from the JSON `text` (inline, a path, or `-`); exits naming the field that is wrong."""
    if text == "-":
        text = sys.stdin.read()
    elif not text.lstrip().startswith("{"):
        text = Path(text).read_text()
    try:
        value: object = json.loads(text)
    except json.JSONDecodeError as e:
        sys.exit(f"bench_commit.py: the input is not JSON: {e}")
    if not isinstance(value, dict):
        sys.exit("bench_commit.py: the input is a JSON object")
    given = cast(dict[str, object], value)
    known = {f.name for f in fields(Input)}
    if unknown := sorted(set(given) - known):
        sys.exit(f"bench_commit.py: no field {', '.join(unknown)} (fields: {', '.join(sorted(known))})")
    if missing := sorted({"message", "paths"} - set(given)):
        sys.exit(f"bench_commit.py: field {', '.join(missing)} is missing")
    message, paths = given["message"], given["paths"]
    if not isinstance(message, str) or not message:
        sys.exit(f"bench_commit.py: field message is a non-empty string, not {json.dumps(message)}")
    items = cast(list[object], paths) if isinstance(paths, list) else []
    strings = tuple(p for p in items if isinstance(p, str))
    if not strings or len(strings) != len(items):
        sys.exit(f"bench_commit.py: field paths is a non-empty list of strings, not {json.dumps(paths)}")
    site = given.get("site", False)
    if not isinstance(site, bool):
        sys.exit(f"bench_commit.py: field site is a bool, not {json.dumps(site)}")
    attempts = given.get("attempts", 5)
    if not isinstance(attempts, int) or isinstance(attempts, bool) or attempts < 1:
        sys.exit(f"bench_commit.py: field attempts is a positive int, not {json.dumps(attempts)}")
    return Input(message=message, paths=strings, site=site, attempts=attempts)


class StepError(Exception):
    """A step that failed: what it was, the command, its exit code and the end of its stderr."""


async def run(step: str, *argv: str, check: bool = True, seconds: float = 300) -> tuple[int, str]:
    """Run `argv` for `step`; its exit code and stdout. Raises StepError on a non-zero exit when
    `check`, and on taking longer than `seconds` always."""
    proc = await asyncio.create_subprocess_exec(
        *argv, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE
    )
    try:
        async with asyncio.timeout(seconds):
            out, err = await proc.communicate()
    except TimeoutError:
        proc.kill()
        await proc.wait()
        raise StepError(f"{step}: `{' '.join(argv)}` took over {seconds:.0f} s") from None
    code = proc.returncode if proc.returncode is not None else -1
    if check and code:
        tail = err.decode(errors="replace").strip()[-2000:]
        raise StepError(f"{step}: `{' '.join(argv)}` exited {code}: {tail}")
    return code, out.decode(errors="replace")


def is_drawn(path: str) -> bool:
    return path in DRAWN or (path.startswith(CHARTS + "trend-") and path.endswith(".svg"))


async def draw() -> None:
    await run("draw the trends", sys.executable, "harness/benchcharts.py", "{}")


async def staged_drawn() -> bool:
    """Stage what drawing changed; whether anything did."""
    await run("stage the trends", "git", "add", "-A", "--", *DRAWN, CHARTS)
    code, _ = await run("compare the trends", "git", "diff", "--cached", "--quiet", check=False)
    return code != 0


async def rebase() -> None:
    """Put the commit on origin/main, settling a conflict only in drawn files by drawing them again."""
    code, _ = await run("rebase onto main", "git", "rebase", "-q", "origin/main", check=False)
    if code == 0:
        return
    _, out = await run("list the conflicts", "git", "diff", "--name-only", "--diff-filter=U")
    conflicted = sorted(out.split())
    if not conflicted or not all(is_drawn(p) for p in conflicted):
        await run("abort the rebase", "git", "rebase", "--abort", check=False)
        raise StepError(
            f"rebase onto main: the commit conflicts with main in {', '.join(conflicted) or 'no file named'}, "
            "which are not only drawn trend files"
        )
    await draw()
    left = [
        p
        for p in conflicted
        if p.endswith((".md", ".svg"))
        and Path(p).exists()
        and any(line.startswith(MARKERS) for line in Path(p).read_text().splitlines())
    ]
    if left:
        await run("abort the rebase", "git", "rebase", "--abort", check=False)
        raise StepError(f"rebase onto main: conflicts outside the trend sections in {', '.join(left)}")
    await staged_drawn()
    await run("continue the rebase", "git", "-c", "core.editor=true", "rebase", "--continue")


async def commit(given: Input) -> None:
    await run("name the bot", "git", "config", "user.name", BOT[0])
    await run("name the bot", "git", "config", "user.email", BOT[1])
    await run("stage the results", "git", "add", "-A", "--", *given.paths)
    code, _ = await run("compare the results", "git", "diff", "--cached", "--quiet", check=False)
    if code == 0:
        print("nothing changed: nothing to commit")
        return
    await run("commit the results", "git", "commit", "-q", "-m", given.message)
    for attempt in range(1, given.attempts + 1):
        await run("fetch main", "git", "fetch", "-q", "origin", "main")
        await rebase()
        await draw()
        if await staged_drawn():
            await run("amend with the trends", "git", "commit", "-q", "--amend", "--no-edit")
        code, _ = await run("push to main", "git", "push", "-q", "origin", "HEAD:main", check=False)
        if code == 0:
            print(f"pushed on attempt {attempt}")
            if given.site:
                await run("start the site's deploy", "gh", "workflow", "run", "site.yaml", "--ref", "main")
            return
        print(
            f"push refused on attempt {attempt} of {given.attempts}: main moved, trying again",
            file=sys.stderr,
        )
        await asyncio.sleep(attempt * 5)
    raise StepError(f"push to main: refused {given.attempts} times")


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    try:
        asyncio.run(commit(typed(sys.argv[1])))
    except StepError as e:
        print(f"::error::bench_commit.py: {e}")
        sys.exit(1)


if __name__ == "__main__":
    main()
