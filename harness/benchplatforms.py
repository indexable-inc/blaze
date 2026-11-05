#!/usr/bin/env python3
"""benchplatforms.py: the platforms the README's benchmarks are measured on, and the runner of each.

    benchplatforms.py matrix [--bun]

Every benchmark of the README is a matrix over PLATFORMS, in this order. Each platform is one
machine of its own, a self-hosted runner whose labels are `labels`: its numbers are comparable
with each other and with its own history, not with another platform's. A result says which
platform it is by its host (`platform()`); benchreport.py stores it as
docs/benchmarks/<slug>.json (runtime-<slug>.json, bun-<slug>.json, ...) and writes every README
table from the newest stored result of each platform, so a platform that did not run this time
keeps its last numbers, with their date and commit.

`matrix` prints, for the bench-readme and bench-bun workflows, `matrix=` (the platforms whose
runner is registered, as a strategy matrix) and `any=true|false` to $GITHUB_OUTPUT, or to stdout
without it. A runner is registered when the repository variable BENCH_RUNNERS (passed in the
environment) names its slug, comma-separated: `linux-x86-64,linux-aarch64,macos-arm64`.
BENCH_RUNNER=true, the variable before there were platforms, means Linux x86-64.
"""
import json, os, sys

# slug, name, the runner's labels, and where the work goes: a short path on macOS, where the
# daemon's socket path has to fit in 104 bytes.
PLATFORMS = [
    dict(slug="macos-arm64", name="macOS arm64", labels=["self-hosted", "macOS", "ARM64", "bench-macos"], work="/tmp/blaze-bench"),
    dict(slug="linux-aarch64", name="Linux aarch64", labels=["self-hosted", "linux", "ARM64", "bench-arm64"], work=""),
    dict(slug="linux-x86-64", name="Linux x86-64", labels=["self-hosted", "linux", "x64", "bench-x86"], work=""),
]
NAMES = [p["name"] for p in PLATFORMS]


def platform(result):
    """The platform of a result of bench.py, from its host: one of NAMES, or what the host says."""
    host = result.get("host") or {}
    system = {"Linux": "Linux", "Darwin": "macOS"}.get(host.get("os"), host.get("os") or "unknown OS")
    arch = host.get("arch") or ""
    if arch in ("x86_64", "AMD64"):
        arch = "x86-64"
    elif arch in ("arm64", "aarch64"):
        arch = "arm64" if system == "macOS" else "aarch64"
    return f"{system} {arch}".strip()


def slug(name):
    return name.replace(" ", "-").lower()


def registered(env):
    named = {s.strip() for s in (env.get("BENCH_RUNNERS") or "").split(",") if s.strip()}
    if env.get("BENCH_RUNNER") == "true":
        named.add("linux-x86-64")
    return [p for p in PLATFORMS if p["slug"] in named]


def main():
    argv = sys.argv[1:]
    if argv[:1] != ["matrix"]:
        raise SystemExit(__doc__)
    found = registered(os.environ)
    lines = [f"matrix={json.dumps({'include': found})}", f"any={'true' if found else 'false'}"]
    out = os.environ.get("GITHUB_OUTPUT")
    with open(out, "a") if out else sys.stdout as f:
        f.write("\n".join(lines) + "\n")
    skipped = [p["name"] for p in PLATFORMS if p not in found]
    if skipped:
        print(f"not registered, so not run (the README keeps their last numbers): {', '.join(skipped)}", file=sys.stderr)


if __name__ == "__main__":
    main()
