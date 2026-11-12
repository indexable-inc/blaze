#!/usr/bin/env python3
"""benchplatforms.py: the platforms the README's benchmarks are measured on, and the runner of each.

    benchplatforms.py '{"workflow": "bench-readme", "runners": "linux-x86-64,windows-x86-64"}'

Every benchmark of the README is a matrix over PLATFORMS, in this order. Each platform is one
machine of its own, a self-hosted runner whose labels are `labels`: its numbers are comparable
with each other and with its own history, not with another platform's. A result says which
platform it is by its host (`platform()`); benchreport.py stores it as
docs/benchmarks/<slug>.json (runtime-<slug>.json, bun-<slug>.json, ...) and writes every README
table from the newest stored result of each platform, so a platform that did not run this time
keeps its last numbers, with their date and commit.

The input is one JSON object (`Input`): `workflow`, the bench workflow asking, and `runners`,
the repository variable BENCH_RUNNERS, the slugs of the platforms whose runner is registered,
comma-separated (`runner`, BENCH_RUNNER, `"true"` from before there were platforms, means Linux
x86-64). It prints `matrix=` (the registered platforms the workflow runs on, as a strategy
matrix, each with the projects bench-readme measures there) and `any=true|false` to
$GITHUB_OUTPUT, or to stdout without it.
"""
import json, os, sys
from dataclasses import dataclass
from typing import Literal

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), os.pardir, "tools", "typed-input", "src"))
from typed_input import parse

# The projects bench-readme measures on a platform (bench.py's PROJECTS).
EVERY = ["ripgrep", "fd", "bat", "hyperfine", "just", "axum", "nushell", "helix", "bevy", "biome"]
UNIX = ["bench-readme", "bench-bun", "bench-counts"]

# slug, name, the runner's labels, where the work goes (a short path on macOS, where the
# daemon's socket path has to fit in 104 bytes), the projects bench-readme measures, and the
# workflows that run there.
PLATFORMS = [
    dict(slug="macos-arm64", name="macOS arm64", labels=["self-hosted", "macOS", "ARM64", "bench-macos"], work="/tmp/blaze-bench",
         projects=EVERY, workflows=UNIX),
    dict(slug="linux-aarch64", name="Linux aarch64", labels=["self-hosted", "linux", "ARM64", "bench-arm64"], work="",
         projects=EVERY, workflows=UNIX),
    dict(slug="linux-x86-64", name="Linux x86-64", labels=["self-hosted", "linux", "x64", "bench-x86"], work="",
         projects=EVERY, workflows=UNIX),
    # MSVC, in Git Bash with the Build Tools' environment and no Nix. ripgrep and fd first, the
    # others as each is shown to build there. Not bench-bun (bun's C and C++ build, bunprep.py,
    # is Nix's) nor bench-counts (Windows has no instruction counts a user can read: counters.py).
    dict(slug="windows-x86-64", name="Windows x86-64", labels=["self-hosted", "windows", "x64", "bench-windows"], work="",
         projects=["ripgrep", "fd"], workflows=["bench-readme"]),
]
NAMES = [p["name"] for p in PLATFORMS]


@dataclass(frozen=True)
class Input:
    workflow: Literal["bench-readme", "bench-bun", "bench-counts"]
    runners: str = ""
    runner: str = ""


def platform(result):
    """The platform of a result of bench.py, from its host: one of NAMES, or what the host says."""
    host = result.get("host") or {}
    system = {"Linux": "Linux", "Darwin": "macOS", "Windows": "Windows"}.get(host.get("os"), host.get("os") or "unknown OS")
    arch = host.get("arch") or ""
    if arch in ("x86_64", "AMD64"):
        arch = "x86-64"
    elif arch in ("arm64", "aarch64"):
        arch = "arm64" if system == "macOS" else "aarch64"
    return f"{system} {arch}".strip()


def slug(name):
    return name.replace(" ", "-").lower()


def registered(it):
    """The platforms `it.workflow` runs on whose runner `it` says is registered."""
    named = {s.strip() for s in it.runners.split(",") if s.strip()}
    if it.runner == "true":
        named.add("linux-x86-64")
    if unknown := named - {p["slug"] for p in PLATFORMS}:
        raise SystemExit(f"benchplatforms.py: BENCH_RUNNERS names {', '.join(sorted(unknown))}, which is no platform "
                         f"({', '.join(p['slug'] for p in PLATFORMS)})")
    return [p for p in PLATFORMS if p["slug"] in named and it.workflow in p["workflows"]]


def main():
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    it = parse(Input, sys.argv[1], "benchplatforms.py")
    found = registered(it)
    lines = [f"matrix={json.dumps({'include': found})}", f"any={'true' if found else 'false'}"]
    out = os.environ.get("GITHUB_OUTPUT")
    with open(out, "a") if out else sys.stdout as f:
        f.write("\n".join(lines) + "\n")
    skipped = [p["name"] for p in PLATFORMS if p not in found and it.workflow in p["workflows"]]
    if skipped:
        print(f"not registered, so not run (the README keeps their last numbers): {', '.join(skipped)}", file=sys.stderr)


if __name__ == "__main__":
    main()
