"""Fail when blaze and harness pin different flecs revs.

Both build the indexable-inc/flecs fork's amalgamation through its `flecs-distr` crate, pinned by
git rev: blaze in vendor/flecs-c/Cargo.toml, harness (its own cargo workspace) in
crates/harness/vendor/hyperion-flecs-c/Cargo.toml. The two lines cannot be one, so this keeps them
equal. It reads the two manifests only: no cargo, no build. blaze.yaml's `changes` job runs it.

    python3 .github/scripts/flecs_pin.py
"""

import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MANIFESTS = ("vendor/flecs-c/Cargo.toml", "crates/harness/vendor/hyperion-flecs-c/Cargo.toml")
REPO = "https://github.com/indexable-inc/flecs"


def pin(manifest: str) -> tuple[str, str]:
    path = ROOT / manifest
    dep = tomllib.loads(path.read_text())["build-dependencies"].get("flecs-distr")
    if not isinstance(dep, dict) or "rev" not in dep or "git" not in dep:
        raise SystemExit(f"{manifest}: no `flecs-distr = {{ git = ..., rev = ... }}` build-dependency")
    return str(dep["git"]), str(dep["rev"])


def main() -> None:
    pins = {m: pin(m) for m in MANIFESTS}
    for manifest, (git, _) in pins.items():
        if git.removesuffix(".git") != REPO:
            raise SystemExit(f"{manifest}: flecs-distr comes from {git}, not {REPO}")
    revs = {rev for _, rev in pins.values()}
    if len(revs) != 1:
        lines = "\n".join(f"  {m}: {rev}" for m, (_, rev) in pins.items())
        raise SystemExit(f"blaze and harness pin different flecs revs; set both to one:\n{lines}")
    print(f"flecs pin: {revs.pop()} in {', '.join(MANIFESTS)}")


if __name__ == "__main__":
    sys.exit(main())
