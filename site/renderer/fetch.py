# /// script
# requires-python = ">=3.13"
# ///
"""Fetches the renderer's wasm modules (site.wasm, site-snapshot.wasm) into renderer/target/site/, where gen/build.ts
looks, so the site builds and `wrangler dev` runs on a machine without the Rust toolchain.

    uv run --script renderer/fetch.py                          # from site/: the site workflow's latest green run
    uv run --script renderer/fetch.py --branch main            #   on this branch (default: the current one, then main)
    uv run --script renderer/fetch.py --scp HOST:DIR [--port N]  # from a machine that ran renderer/build.py

The CI artifact is `site-renderer` (.github/workflows/site.yaml), built from that run's commit: when you have changed
renderer/ yourself, build in an ix VM and copy renderer/target/site out with `ix shell` (site/renderer/README.md).
Needs `gh` (signed in) or `scp`.
"""

import argparse
import asyncio
import json
import shutil
import sys
import tempfile
from pathlib import Path

RENDERER = Path(__file__).resolve().parent
OUT = RENDERER / "target" / "site"
FILES = ("site.wasm", "site-snapshot.wasm")
REPO = "indexable-inc/blaze"
ARTIFACT = "site-renderer"


class StepError(Exception):
    pass


async def run(step: str, *cmd: str, bound: float = 300) -> str:
    """Runs `cmd`; on failure names the step, the command, its exit code and the stderr tail."""
    proc = await asyncio.create_subprocess_exec(*cmd, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    try:
        async with asyncio.timeout(bound):
            out, err = await proc.communicate()
    except TimeoutError:
        proc.kill()
        await proc.wait()
        raise StepError(f"{step}: {' '.join(cmd)}: no exit after {bound:.0f} s") from None
    if proc.returncode != 0:
        tail = "\n".join(err.decode(errors="replace").splitlines()[-30:])
        raise StepError(f"{step}: {' '.join(cmd)}: exit {proc.returncode}\n{tail}")
    return out.decode(errors="replace")


async def green_run(branch: str) -> dict | None:
    out = await run(
        f"find a green run on {branch}",
        "gh", "run", "list", "-R", REPO, "--workflow", "site.yaml", "--branch", branch, "--status", "success",
        "--limit", "1", "--json", "databaseId,headSha,headBranch,createdAt",
    )  # fmt: skip
    runs = json.loads(out)
    return runs[0] if runs else None


async def from_ci(branch: str | None, into: Path) -> str:
    if branch is None:
        current = (await run("current branch", "git", "-C", str(RENDERER), "branch", "--show-current")).strip()
        branches = [b for b in (current, "main") if b]
    else:
        branches = [branch]
    for b in dict.fromkeys(branches):
        if r := await green_run(b):
            await run(
                "download the artifact",
                "gh", "run", "download", str(r["databaseId"]), "-R", REPO, "-n", ARTIFACT, "-D", str(into),
            )  # fmt: skip
            return f"run {r['databaseId']} ({r['headBranch']} {r['headSha'][:10]}, {r['createdAt']})"
    raise StepError(f"no green run of site.yaml with the {ARTIFACT} artifact on {', '.join(branches)}")


async def from_scp(source: str, port: int | None, into: Path) -> str:
    opts = ["-P", str(port)] if port else []
    async with asyncio.TaskGroup() as g:
        for f in FILES:
            g.create_task(run(f"scp {f}", "scp", "-q", *opts, f"{source.rstrip('/')}/{f}", str(into / f)))
    return source


async def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--branch", help="the branch whose latest green run to take (default: the current one, then main)")
    ap.add_argument("--scp", metavar="HOST:DIR", help="copy from DIR on HOST (renderer/target/site of a build.py run)")
    ap.add_argument("--port", type=int, help="ssh port for --scp")
    a = ap.parse_args()
    with tempfile.TemporaryDirectory() as tmp:
        into = Path(tmp)
        where = await (from_scp(a.scp, a.port, into) if a.scp else from_ci(a.branch, into))
        missing = [f for f in FILES if not (into / f).is_file()]
        if missing:
            raise StepError(f"{where}: no {', '.join(missing)}")
        OUT.mkdir(parents=True, exist_ok=True)
        for f in FILES:
            shutil.move(into / f, OUT / f)
    sizes = ", ".join(f"{f} {(OUT / f).stat().st_size} bytes" for f in FILES)
    print(f"fetch.py: {sizes} from {where} into {OUT}")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except* StepError as eg:
        for e in eg.exceptions:
            print(e, file=sys.stderr)
        sys.exit(1)
