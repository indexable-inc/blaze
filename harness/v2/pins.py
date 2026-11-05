"""pins.py: the one home of corpus.json's shape, the real code blaze is judged on (AGENTS.md rule
verify-on-real-projects): the crates.io crates corpus.py tests one at a time, and the whole
workspaces realprojects.py builds and tests, each at an exact commit with the id of its tree.

A workspace's pin is its commit (`rev`) and that commit's git tree id (`tree`), both from the
host; a fetch whose tree is another is refused. Where the project commits no `Cargo.lock`, the
lock is the harness's (`lock: "harness"`, at `locks/<name>.Cargo.lock` beside this file), so
two runs build the same dependencies; blaze resolves one with `realprojects.py`'s
`write_locks`, and that file is then committed. The workspace `blaze` is this repository at the
commit the run names (`repo: "self"`, `rev` and `tree` empty).
"""

from dataclasses import dataclass
from pathlib import Path
from typing import Literal

from typed_input import parse

HERE = Path(__file__).resolve().parent
CORPUS = HERE / "corpus.json"
LOCKS = HERE / "locks"


@dataclass(frozen=True)
class Crate:
    name: str
    version: str


@dataclass(frozen=True)
class Env:
    """One variable a workspace's build and tests are given."""

    name: str
    value: str


@dataclass(frozen=True)
class Workspace:
    """A whole workspace blaze builds and tests: `repo` is its GitHub `owner/name` (or `self`,
    this repository), `tag` what the project calls `rev`, `tree` the git tree id of `rev`.
    `lock` says whose `Cargo.lock` it builds with: the project's own (`committed`, built with
    `--locked`) or the harness's (`harness`, LOCKS). `submodules` checks them out at the commits
    the tree pins. `build_args` and `test_args` come after `--workspace` (a feature set the
    project's own CI tests with); `minutes` bounds each of the build and the tests.
    `sandbox_tests` has realprojects.py add `sandboxed-tests` to `experimental` in
    `[workspace.metadata.blaze]` of its copy's root Cargo.toml (never the fetched source), so
    every test runs in blaze's sandbox."""

    name: str
    repo: str
    tag: str
    rev: str
    tree: str
    lock: Literal["committed", "harness"]
    why: str
    submodules: bool = False
    env: tuple[Env, ...] = ()
    build_args: tuple[str, ...] = ()
    test_args: tuple[str, ...] = ()
    minutes: int = 120
    sandbox_tests: bool = False

    def lock_file(self) -> Path:
        return LOCKS / f"{self.name}.Cargo.lock"


@dataclass(frozen=True)
class Corpus:
    """corpus.json."""

    about: str
    crates: tuple[Crate, ...]
    workspaces: tuple[Workspace, ...]


def load_corpus(path: Path = CORPUS, script: str = "pins.py") -> Corpus:
    return parse(Corpus, str(path), script)
