"""cpus.py: writes blaze's table of the CPUs of `-C target-cpu` and the target features each has.

    uv run --frozen python tools/cpus.py '{"write": true}'
                                 write crates/target-features/src/cpus/table.rs from the data
    uv run --frozen python tools/cpus.py '{"write": true, "fetch_llvm": true}'
                                 first read the pinned LLVM's CPU aliases again (after an LLVM bump)
    uv run --frozen python tools/cpus.py '{}'
                                 check that the table is what the data says (exit 1 if not)

The JSON is inline, a file's path, or `-` for stdin.

The table is the one home of what blaze knows of CPUs (`target_features::cpus`): blaze-units
gives `cfg(target_feature)` by it and the code generator the CPU's features and name. A row is a
CPU of a target and the `target_feature`s rustc gives it, from two sources:

- the CPUs `rustc --print target-cpus` lists and the features `--print cfg -C target-cpu=<cpu>`
  prints, which harness/answers.py records (`cpus` and `cpu_features` of each target of
  crates/blaze-units/src/stdlib/rustc.json, and of blaze's own targets in own_targets.json);
- the aliases of those CPUs in the LLVM `crates/hir-ty-codegen/llvm.json` pins
  (crates/target-features/llvm_cpus.json). LLVM keeps an alias (`def : ProcessorAlias<"apple-m1",
  "apple-a14">;`) in a table of its own (`SubtargetSubTypeAliasKV`), which
  `MCSubtargetInfo::getAllProcessorDescriptions` and so rustc's list leave out, while
  `-C target-cpu` takes it; an alias has the features of the CPU it names. `fetch_llvm` reads
  them from the TableGen sources of each target blaze compiles for at the pinned commit (with
  `gh api`).

Run it after `harness/answers.py --write`; toolchain-bump.yaml does after merging each host's
answers, and commits the table with them.
"""

import asyncio
import json
import re
import sys
import typing
from dataclasses import dataclass
from pathlib import Path

from typed_input import parse

SCRIPT = "cpus.py"
ROOT = Path(__file__).resolve().parent.parent
PIN = ROOT / "crates/hir-ty-codegen/llvm.json"
ANSWERS = ROOT / "crates/blaze-units/src/stdlib/rustc.json"
OWN = ROOT / "crates/blaze-units/src/stdlib/own_targets.json"
ALIASES = ROOT / "crates/target-features/llvm_cpus.json"
TABLE = ROOT / "crates/target-features/src/cpus/table.rs"
# LLVM's target directory of each architecture blaze compiles for, by the first part of the
# triple.
LLVM_TARGETS = {"aarch64": "AArch64", "x86_64": "X86", "wasm32": "WebAssembly"}
ALIAS = re.compile(r'^\s*def\s*:\s*ProcessorAlias<\s*"([^"]+)"\s*,\s*"([^"]+)"\s*>\s*;', re.MULTILINE)
# Any other mention of one, which the pattern above would miss: a `foreach`, a `let`.
MENTION = re.compile(r"\bProcessorAlias\b")
STEP_TIMEOUT_S = 120


@dataclass(frozen=True)
class Input:
    write: bool = False
    fetch_llvm: bool = False


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
    """Every TableGen file of the targets in LLVM_TARGETS at the pinned commit (each target's are
    in its directory, none below)."""
    async with asyncio.TaskGroup() as group:
        listed = [
            group.create_task(
                gh(
                    f"list LLVM's {it} sources",
                    f"repos/{at.owner_repo}/contents/llvm/lib/Target/{it}?ref={at.commit}",
                )
            )
            for it in LLVM_TARGETS.values()
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
    mentions = len(MENTION.findall(text)) - len(re.findall(r"^\s*class\s+ProcessorAlias\b", text, re.MULTILINE))
    if mentions != len(found):
        raise Failed(
            f"read {path}: it mentions ProcessorAlias {mentions} times outside its class, and "
            f"{len(found)} are `def : ProcessorAlias<\"name\", \"cpu\">;`: read the others by hand"
        )
    return found


async def fetched_aliases(at: Pin) -> dict[str, object]:
    paths = await td_files(at)
    async with asyncio.TaskGroup() as group:
        tasks = {path: group.create_task(aliases_in(at, path)) for path in paths}
    aliases: dict[str, dict[str, str]] = {arch: {} for arch in LLVM_TARGETS}
    for path, task in tasks.items():
        arch = next(a for a, d in LLVM_TARGETS.items() if path.startswith(f"llvm/lib/Target/{d}/"))
        for name, cpu in task.result():
            if aliases[arch].setdefault(name, cpu) != cpu:
                raise Failed(f"read {path}: alias `{name}` names `{cpu}` and `{aliases[arch][name]}`")
    return {
        "llvm": f"{at.version} {at.commit}",
        "aliases": {arch: dict(sorted(it.items())) for arch, it in sorted(aliases.items())},
    }


def rendered_json(data: dict[str, object]) -> str:
    return json.dumps(data, indent=1, sort_keys=True) + "\n"


def rust_str(text: str) -> str:
    if not re.fullmatch(r"[A-Za-z0-9._+-]+", text):
        raise Failed(f"build the table: `{text}` is no CPU or feature name the table can hold as written")
    return f'"{text}"'


def table(answers: dict[str, object], own: dict[str, object], llvm: dict[str, object]) -> str:
    """The Rust of the table: each target's CPUs, sorted, with LLVM's aliases of them, each by the
    index of its feature set."""
    targets: dict[str, dict[str, object]] = {}
    for source in (answers, own):
        for triple, data in typing.cast(dict[str, dict[str, object]], source["targets"]).items():
            if triple in targets:
                raise Failed(f"build the table: `{triple}` is in both rustc.json and own_targets.json")
            targets[triple] = data
    aliases_by_arch: dict[str, dict[str, str]] = llvm["aliases"]  # pyright: ignore[reportAssignmentType]
    lines = [
        "//! What `tools/cpus.py` wrote from rustc.json's and own_targets.json's `cpus` and",
        f"//! `cpu_features` (rustc {answers['version']}) and the CPU aliases of LLVM {llvm['llvm']}",
        "//! (`llvm_cpus.json`); regenerate, don't edit.",
        "",
        "use super::Cpus;",
        "",
        "/// Each target's CPUs, by triple, sorted.",
        "#[rustfmt::skip]",
        "pub(super) static TARGETS: &[(&str, Cpus)] = &[",
    ]
    for triple in sorted(targets):
        data = targets[triple]
        cpus: dict[str, int] = dict(typing.cast(dict[str, int], data.get("cpus") or {}))
        sets = typing.cast(list[list[str]], data.get("cpu_features") or [])
        for alias, named in aliases_by_arch.get(triple.split("-")[0], {}).items():
            # A name rustc lists itself keeps rustc's features; an alias of a CPU rustc does not
            # list (its LLVM is older) has none to give.
            if alias not in cpus and named in cpus:
                cpus[alias] = cpus[named]
        for name, at in cpus.items():
            if not 0 <= at < len(sets):
                raise Failed(f"build the table: {triple}'s CPU `{name}` has the set {at} of {len(sets)}")
        lines.append(f"    ({rust_str(triple)}, Cpus {{")
        lines.append("        names: &[")
        for name in sorted(cpus):
            lines.append(f"            ({rust_str(name)}, {cpus[name]}),")
        lines.append("        ],")
        lines.append("        sets: &[")
        for features in sets:
            lines.append(f"            &[{', '.join(rust_str(it) for it in sorted(features))}],")
        lines.append("        ],")
        lines.append("    }),")
    lines.append("];")
    return "\n".join(lines) + "\n"


def main() -> int:
    args = parse(Input, sys.argv[1] if len(sys.argv) > 1 else "{}", SCRIPT)
    try:
        if args.fetch_llvm:
            at = pin()
            aliases = rendered_json(asyncio.run(fetched_aliases(at)))
            if args.write:
                ALIASES.write_text(aliases)
                print(f"wrote the CPU aliases of LLVM {at.version} ({at.commit}) to {ALIASES.relative_to(ROOT)}")
            elif ALIASES.read_text() != aliases:
                raise Failed(f"{ALIASES.relative_to(ROOT)} is not what LLVM {at.version} ({at.commit}) says")
        text = table(json.loads(ANSWERS.read_text()), json.loads(OWN.read_text()), json.loads(ALIASES.read_text()))
    except Failed as e:
        print(f"{SCRIPT}: {e}", file=sys.stderr)
        return 1
    if args.write:
        TABLE.parent.mkdir(parents=True, exist_ok=True)
        TABLE.write_text(text)
        print(f"wrote {TABLE.relative_to(ROOT)}")
        return 0
    if not TABLE.exists() or TABLE.read_text() != text:
        print(
            f"{SCRIPT}: {TABLE.relative_to(ROOT)} is not what the data says: run "
            "`uv run --frozen python tools/cpus.py '{\"write\": true}'`",
            file=sys.stderr,
        )
        return 1
    print(f"{TABLE.relative_to(ROOT)} is what the data says")
    return 0


if __name__ == "__main__":
    sys.exit(main())
