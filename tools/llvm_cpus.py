"""llvm_cpus.py: writes the CPU aliases of the LLVM blaze pins into blaze's data.

    uv run --frozen python tools/llvm_cpus.py '{"write": true}'
                                 write crates/blaze-units/src/stdlib/llvm_cpus.json
    uv run --frozen python tools/llvm_cpus.py '{}'
                                 check that the file is what the pinned LLVM says (exit 1 if not)

The JSON is inline, a file's path, or `-` for stdin.

LLVM names some CPUs as aliases of another (`def : ProcessorAlias<"apple-m1", "apple-a14">;`):
they are kept in a table of their own (`SubtargetSubTypeAliasKV`), which
`MCSubtargetInfo::getAllProcessorDescriptions` and so `rustc --print target-cpus` leave out, while
LLVM, and so rustc, accept them for `-C target-cpu`. harness/answers.py records each CPU rustc
lists (rustc.json's `cpus`); this records each alias of the LLVM `crates/hir-ty-codegen/llvm.json`
pins, by architecture, read from the TableGen sources of every target blaze compiles for at the
pinned commit (fetched with `gh api`). blaze-units resolves an alias to the CPU it names before it
looks the CPU's features up. Run it after an LLVM bump.
"""

import asyncio
import json
import re
import sys
from dataclasses import dataclass
from pathlib import Path

from typed_input import parse

SCRIPT = "llvm_cpus.py"
ROOT = Path(__file__).resolve().parent.parent
PIN = ROOT / "crates/hir-ty-codegen/llvm.json"
DATA = ROOT / "crates/blaze-units/src/stdlib/llvm_cpus.json"
# LLVM's target directory of each architecture blaze compiles for, by the first part of the
# triple (blaze-units' `arch`).
TARGETS = {"aarch64": "AArch64", "x86_64": "X86", "wasm32": "WebAssembly"}
ALIAS = re.compile(r'^\s*def\s*:\s*ProcessorAlias<\s*"([^"]+)"\s*,\s*"([^"]+)"\s*>\s*;', re.M)
# Any other mention of one, which the pattern above would miss: a `foreach`, a `let`.
MENTION = re.compile(r"\bProcessorAlias\b")
STEP_TIMEOUT_S = 120


@dataclass(frozen=True)
class Input:
    write: bool = False


@dataclass(frozen=True)
class Pin:
    version: str
    owner_repo: str
    commit: str


class Failed(Exception):
    pass


def pin() -> Pin:
    raw = json.loads(PIN.read_text())
    repo = raw["repo"].removeprefix("https://github.com/").removesuffix(".git")
    return Pin(version=raw["version"], owner_repo=repo, commit=raw["commit"])


async def gh(step: str, *args: str) -> bytes:
    """`gh api` with `args`, its stdout; a failure names the step, the command, its exit code and
    the tail of its stderr."""
    argv = ["gh", "api", *args]
    async with asyncio.timeout(STEP_TIMEOUT_S):
        ran = await asyncio.create_subprocess_exec(
            *argv, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE
        )
        out, err = await ran.communicate()
    if ran.returncode != 0:
        tail = err.decode(errors="replace")[-2000:]
        raise Failed(f"{step}: `{' '.join(argv)}` exited {ran.returncode}: {tail}")
    return out


async def td_files(at: Pin) -> list[str]:
    """Every TableGen file of the targets in TARGETS at the pinned commit (each target's are in
    its directory, none below)."""
    async with asyncio.TaskGroup() as group:
        listed = [
            group.create_task(
                gh(
                    f"list LLVM's {it} sources",
                    f"repos/{at.owner_repo}/contents/llvm/lib/Target/{it}?ref={at.commit}",
                )
            )
            for it in TARGETS.values()
        ]
    return sorted(
        entry["path"]
        for task in listed
        for entry in json.loads(task.result())
        if entry["type"] == "file" and entry["path"].endswith(".td")
    )


async def aliases_in(at: Pin, path: str) -> list[tuple[str, str]]:
    text = (
        await gh(
            f"read {path}",
            "-H",
            "Accept: application/vnd.github.raw",
            f"repos/{at.owner_repo}/contents/{path}?ref={at.commit}",
        )
    ).decode()
    found = ALIAS.findall(text)
    # Each mention is the class itself or one alias the pattern read; another is one it can't.
    mentions = len(MENTION.findall(text)) - len(re.findall(r"^\s*class\s+ProcessorAlias\b", text, re.M))
    if mentions != len(found):
        raise Failed(
            f"read {path}: it mentions ProcessorAlias {mentions} times outside its class, and "
            f"{len(found)} are `def : ProcessorAlias<\"name\", \"cpu\">;`: read the others by hand"
        )
    return found


async def table(at: Pin) -> dict[str, object]:
    paths = await td_files(at)
    async with asyncio.TaskGroup() as group:
        tasks = {path: group.create_task(aliases_in(at, path)) for path in paths}
    aliases: dict[str, dict[str, str]] = {arch: {} for arch in TARGETS}
    for path, task in tasks.items():
        arch = next(a for a, d in TARGETS.items() if path.startswith(f"llvm/lib/Target/{d}/"))
        for name, cpu in task.result():
            if aliases[arch].setdefault(name, cpu) != cpu:
                raise Failed(f"read {path}: alias `{name}` names `{cpu}` and `{aliases[arch][name]}`")
    return {
        "llvm": f"{at.version} {at.commit}",
        "aliases": {arch: dict(sorted(it.items())) for arch, it in sorted(aliases.items())},
    }


def rendered(data: dict[str, object]) -> str:
    return json.dumps(data, indent=1, sort_keys=True) + "\n"


def main() -> int:
    args = parse(Input, sys.argv[1] if len(sys.argv) > 1 else "{}", SCRIPT)
    at = pin()
    try:
        text = rendered(asyncio.run(table(at)))
    except Failed as e:
        print(f"{SCRIPT}: {e}", file=sys.stderr)
        return 1
    if args.write:
        DATA.write_text(text)
        print(f"wrote the CPU aliases of LLVM {at.version} ({at.commit}) to {DATA.relative_to(ROOT)}")
        return 0
    if not DATA.exists() or DATA.read_text() != text:
        print(
            f"{SCRIPT}: {DATA.relative_to(ROOT)} is not what LLVM {at.version} ({at.commit}) says: "
            f"run `uv run --frozen python tools/llvm_cpus.py '{{\"write\": true}}'`",
            file=sys.stderr,
        )
        return 1
    print(f"{DATA.relative_to(ROOT)} is LLVM {at.version}'s ({at.commit})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
