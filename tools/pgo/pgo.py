#!/usr/bin/env python3
"""blaze built with its own profiles: PGO for every target, BOLT on ELF ones.

One typed input (AGENTS.md rule scripts-take-typed-input), one of two kinds:

  uv run --frozen python tools/pgo/pgo.py '{"kind": "build", "target": "x86_64-unknown-linux-gnu",
      "builder": "/path/to/blaze", "out": "out"}'
  uv run --frozen python tools/pgo/pgo.py '{"kind": "publish", "dirs": ["out-x86_64", "out-aarch64"]}'

`build`, run in an ix VM of the release train, one per target:

  1. fetches the pinned LLVM (crates/hir-ty-codegen/llvm.json) through .github/scripts/llvm.py,
     whose prefix also has llvm-profdata, llvm-bolt, merge-fdata and perf2bolt;
  2. builds blaze instrumented (`-Cprofile-generate`) with `builder`, which is blaze: blaze builds
     blaze, never cargo or rustc;
  3. runs that blaze's daemon on the fixed training set of tools/pgo/training.json (blaze itself,
     ripgrep, nushell and the crates of harness/v2/corpus.json: cold builds, no-op builds, edits,
     tests), each turn of which must succeed;
  4. merges the raw profiles and refuses a profile under `min_count` counts;
  5. builds blaze again with the profile (`-Cprofile-use`);
  6. on an ELF target, runs BOLT (`llvm-bolt` of the pinned LLVM) on the binaries of `bolt_bins`,
     trained either by BOLT's instrumentation (the default: a VM has no LBR) or by `perf record`
     with branch stacks, then optimised;
  7. runs the gate with the optimised binaries as BLAZE_BIN;
  8. times the turns of the training's `measure` projects, `measure_runs` rounds, with a plain
     build of this commit (`baseline`, or built here without a profile), the PGO build and the
     PGO and BOLT build in turn, and refuses an optimised blaze that saves under `min_speedup`
     thousandths of the plain one's least time: a profile that did not apply stops the release;
  9. writes into `out` the merged profile, the BOLT profile and the binaries, each named by its
     content's sha256, and `<target>.json`, its entry for tools/pgo/profiles.json, with the
     speedup measured.

`publish` checks every file each `<target>.json` of `dirs` names against its sha256, uploads them
to the release tools/pgo/profiles.json names (`upload`), and writes their entries into it.

Every step is bounded and a failure stops the script naming the step; nothing is skipped or
replaced by another path: a target BOLT cannot work on is refused when it asks for BOLT, and an
ELF target that asks for none is refused too.
"""

import dataclasses
import hashlib
import io
import json
import math
import os
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tarfile
import time
import tomllib
import typing
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Literal, NoReturn

import typed_input

SCRIPT = "tools/pgo/pgo.py"
ROOT = Path(__file__).resolve().parents[2]
PINS = ROOT / "tools" / "pgo" / "profiles.json"
TOOLCHAIN = ROOT / "harness" / "toolchain"
LLVM_PIN = ROOT / "crates" / "hir-ty-codegen" / "llvm.json"
LLVM_PY = ROOT / ".github" / "scripts" / "llvm.py"
ENGINE = ROOT / "crates" / "blaze-engine"
PACKAGE = "blaze-engine"
PROFILE = "dist"
INDEX = "https://index.crates.io/"
DOWNLOAD = "https://static.crates.io/crates/{name}/{name}-{version}.crate"
FETCH_SECS = 120.0
# How long the daemon of a training stage may take to exit after `blaze stop` and write its
# profile.
EXIT_SECS = 300.0
SHA256 = re.compile(r"[0-9a-f]{64}")
COMMIT = re.compile(r"[0-9a-f]{40}")
NAME = r"[A-Za-z0-9_.-]+"
RELEASE = re.compile(rf"https://github\.com/({NAME}/{NAME})/releases/download/({NAME})")
BOLT_TOOLS = ("llvm-bolt", "merge-fdata", "perf2bolt")
GATE = (("uv", "run", "--frozen", "python", "harness/ci.py", "run", '{"part": "fast"}'),)


class Failed(Exception):
    """A step that did not do what it must, naming it."""


@dataclass(frozen=True)
class Build:
    kind: Literal["build"]
    # The Rust target triple blaze is built for, which must be the host's.
    target: str
    # The blaze that builds blaze.
    builder: str
    # Where the outputs go; it must not exist yet, or be empty.
    out: str
    # Where the sources, target directories and raw profiles go; `out` + ".work" when empty.
    work: str = ""
    training: str = "tools/pgo/training.json"
    bolt: Literal["instrument", "perf", "none"] = "instrument"
    # The binaries BOLT optimises: the engine, which every command runs.
    bolt_bins: tuple[str, ...] = ("blaze-engine",)
    # Each command of the gate, run in the repository with BLAZE_BIN set to the optimised engine.
    gate: tuple[tuple[str, ...], ...] = GATE
    min_count: int = 1_000_000_000
    turn_secs: int = 3600
    build_secs: int = 7200
    # The least the optimised blaze must save of a plain build's time on the measured turns, in
    # thousandths. PGO saves rustc 100 to 160 and BOLT 20 to 50 more; 50 still passes on a noisy
    # VM and stops a profile that applied to nothing.
    min_speedup: int = 50
    # The rounds each set of binaries runs the measured turns; the least time of each is kept.
    measure_runs: int = 2
    # A directory of this commit's blaze built plainly (the `dist` profile and the `llvm`
    # feature, no profile), the speedup's baseline; empty builds one here.
    baseline: str = ""


@dataclass(frozen=True)
class Publish:
    kind: Literal["publish"]
    # Each the `out` of a `build`.
    dirs: tuple[str, ...]
    upload: bool = True


@dataclass(frozen=True)
class Turn:
    # blaze's arguments: `["build"]`, `["test", "-p", "x"]`.
    args: tuple[str, ...]
    # Whether the project's edit file gets an edit before the turn.
    edit: bool = False


@dataclass(frozen=True)
class Project:
    name: str
    # `self`: this repository at HEAD; `git`: `repo` at `rev`.
    source: Literal["self", "git"]
    # The file the edits go to, relative to the project.
    edit: str
    turns: tuple[Turn, ...]
    repo: str = ""
    rev: str = ""


@dataclass(frozen=True)
class Corpus:
    # The corpus file: `{"about": ..., "crates": [{"name": ..., "version": ...}]}`.
    path: str
    edit: str
    turns: tuple[Turn, ...]
    # The crates of the corpus trained on, by name; every crate when empty.
    crates: tuple[str, ...] = ()


@dataclass(frozen=True)
class Training:
    projects: tuple[Project, ...]
    corpus: Corpus | None = None
    # The projects whose turns are timed for the speedup, by name: at least one.
    measure: tuple[str, ...] = ()


@dataclass(frozen=True)
class CorpusCrate:
    name: str
    version: str


@dataclass(frozen=True)
class CorpusFile:
    about: str
    crates: tuple[CorpusCrate, ...]


# The entries of tools/pgo/profiles.json.


@dataclass(frozen=True)
class Asset:
    name: str
    sha256: str


@dataclass(frozen=True)
class Bolt:
    mode: Literal["instrument", "perf"]
    name: str
    sha256: str


@dataclass(frozen=True)
class Speedup:
    """What the optimised blaze saved on the measured turns against a plain build of its commit,
    in thousandths of the plain build's least time: by the profile alone, and by the profile and
    BOLT (the same on a target BOLT does not rewrite)."""

    plain_ms: int
    pgo: int
    total: int
    runs: int


@dataclass(frozen=True)
class Pin:
    """One target's profile. An entry of the cargo-trained pipeline (.github/scripts/build_release.py)
    has no `builder`, `llvm`, `bolt`, `binaries` or `speedup`: it is `cargo`'s."""

    name: str
    sha256: str
    commit: str
    count: int
    crate: str | None
    builder: Literal["cargo", "blaze"] = "cargo"
    # The commit of the pinned LLVM the profile was made by: a profile is read by that LLVM only.
    llvm: str | None = None
    bolt: Bolt | None = None
    binaries: Asset | None = None
    speedup: Speedup | None = None


@dataclass(frozen=True)
class Pins:
    toolchain: str
    release: str
    # By target, sorted.
    targets: tuple[tuple[str, Pin], ...]


Input = Build | Publish


def fail(message: str) -> NoReturn:
    typed_input.fail(SCRIPT, message)


def wrong(message: str) -> NoReturn:
    raise typed_input.InputError(message)


def of[T](cls: type[T], value: object, where: str) -> T:
    """`value` read as `cls`; exits naming `where` and the field that is wrong."""
    return typed_input.parse(cls, json.dumps(value), f"{SCRIPT}: {where}")


def parse_input(text: str) -> Input:
    """The input of `text`: inline JSON, a path or `-`."""
    value = typed_input.read(text)
    kind = value.get("kind") if isinstance(value, dict) else None
    if kind == "build":
        built = of(Build, value, "the input")
        check_build(built)
        return built
    if kind == "publish":
        return of(Publish, value, "the input")
    wrong(f'the input is an object whose kind is "build" or "publish", not {json.dumps(value)}')


def is_elf(target: str) -> bool:
    """Whether `target`'s binaries are ELF, the one format BOLT rewrites."""
    return not any(part in target for part in ("-apple-", "-windows", "-uefi", "wasm"))


def check_build(build: Build) -> None:
    if build.bolt == "none" and is_elf(build.target):
        wrong(f"field bolt is none, and {build.target} is ELF: its binaries go through BOLT")
    if build.bolt != "none" and not is_elf(build.target):
        wrong(f"field bolt is {build.bolt}, and BOLT rewrites ELF only, not {build.target}")
    if not build.bolt_bins and build.bolt != "none":
        wrong("field bolt_bins is empty, and BOLT needs a binary")
    if build.min_count < 1:
        wrong("field min_count is at least 1")
    if not build.gate:
        wrong("field gate is empty: the optimised binary passes a gate before it is published")
    if not 1 <= build.min_speedup < 1000:
        wrong("field min_speedup is 1 to 999 thousandths: an optimised blaze shows its speedup")
    if build.measure_runs < 1:
        wrong("field measure_runs is at least 1")


def parse_training(text: str) -> Training:
    training = of(Training, typed_input.read(text), "the training")
    names = [p.name for p in training.projects]
    if len(set(names)) != len(names):
        wrong(f"the projects' names repeat: {names}")
    for i, project in enumerate(training.projects):
        where = f"projects[{i}]"
        if not re.fullmatch(r"[a-z0-9][a-z0-9_-]*", project.name):
            wrong(f"{where}.name is lowercase letters, digits, - and _, not {project.name!r}")
        pinned = project.repo.startswith("https://") and COMMIT.fullmatch(project.rev)
        if project.source == "git" and not pinned:
            wrong(f"{where} is a git project: repo is an https URL and rev a full commit")
        if project.source == "self" and (project.repo or project.rev):
            wrong(f"{where} is this repository: it has no repo or rev")
        check_turns(where, project.edit, project.turns)
    if training.corpus is not None:
        check_turns("corpus", training.corpus.edit, training.corpus.turns)
    if not training.measure:
        wrong("measure is empty: the speedup is timed on at least one project")
    if unknown := sorted(set(training.measure) - set(names)):
        wrong(f"measure names {unknown}, which are not projects")
    if len(set(training.measure)) != len(training.measure):
        wrong(f"measure names a project twice: {list(training.measure)}")
    return training


def check_turns(where: str, edit: str, turns: tuple[Turn, ...]) -> None:
    if not turns:
        wrong(f"{where}.turns is empty")
    if Path(edit).is_absolute() or ".." in Path(edit).parts or not edit:
        wrong(f"{where}.edit is a path inside the project, not {edit!r}")
    for i, turn in enumerate(turns):
        if not turn.args:
            wrong(f"{where}.turns[{i}].args is empty")


# Profiles and their bookkeeping.


def sha256_of(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def content_name(stem: str, digest: str, suffix: str) -> str:
    """The name a file of content `digest` is published as: a rebuild never replaces an asset a
    pin names."""
    if not SHA256.fullmatch(digest):
        raise Failed(f"{digest!r} is not a sha256")
    return f"{stem}-{digest[:16]}{suffix}"


def named(path: Path, stem: str, suffix: str) -> Asset:
    """`path` moved to the name of its content beside itself."""
    digest = sha256_of(path)
    name = content_name(stem, digest, suffix)
    path.replace(path.with_name(name))
    return Asset(name, digest)


def profile_count(show: str) -> int:
    """The total count `llvm-profdata show` prints."""
    found = re.search(r"^Total count: (\d+)$", show, re.MULTILINE)
    if found is None:
        raise Failed("llvm-profdata show printed no total count")
    return int(found.group(1))


def encoded_rustflags(flags: list[str]) -> str:
    """`CARGO_ENCODED_RUSTFLAGS`: flags apart by 0x1f, so a path with a space stays one flag."""
    for flag in flags:
        if "\x1f" in flag:
            raise Failed(f"the flag {flag!r} has a 0x1f in it")
    return "\x1f".join(flags)


def stage_flags(target: str, stage: Literal["generate", "use"], profile: Path) -> list[str]:
    """blaze's flags for a stage: instrumented into `profile` (a directory), or using `profile`
    (the merged file). The binary BOLT gets keeps its relocations."""
    if stage == "generate":
        return [f"-Cprofile-generate={profile}"]
    flags = [f"-Cprofile-use={profile}"]
    if is_elf(target):
        flags.append("-Clink-arg=-Wl,--emit-relocs")
    return flags


def binaries(manifest: str, bins: list[str]) -> list[str]:
    """The binaries the package of `manifest` builds: its `[[bin]]`s and, unless `autobins` is
    off, the files of `src/bin` (`bins`, their stems)."""
    package = tomllib.loads(manifest)
    names = [b["name"] for b in package.get("bin", [])]
    if package.get("package", {}).get("autobins", True):
        names += [b for b in bins if b not in names]
    return sorted(names)


def edit_text(n: int) -> str:
    """The `n`th edit appended to a project's edit file: an item no lint fires on, which changes
    the crate's code."""
    return f"\nconst _: u64 = {n};\n"


def load_pins(text: str) -> Pins:
    """tools/pgo/profiles.json, refusing a field it does not have."""
    value = json.loads(text)
    if not isinstance(value, dict):
        wrong("profiles.json is a JSON object")
    fields = typing.cast(dict[str, object], value)
    if sorted(fields) != ["release", "targets", "toolchain"]:
        wrong(f"profiles.json has the fields release, targets, toolchain, not {sorted(fields)}")
    toolchain, release, targets = fields["toolchain"], fields["release"], fields["targets"]
    if not isinstance(toolchain, str) or not isinstance(release, str) or not isinstance(targets, dict):
        wrong("profiles.json: toolchain and release are strings, targets an object")
    if RELEASE.fullmatch(release) is None:
        wrong(f"profiles.json: release is a GitHub release download URL, not {release!r}")
    by_target = typing.cast(dict[str, object], targets)
    pins = tuple((t, read_pin(t, by_target[t])) for t in sorted(by_target))
    return Pins(toolchain, release, pins)


def read_pin(target: str, value: object) -> Pin:
    pin = of(Pin, value, f"targets.{target}")
    for asset in files_of(pin):
        if not SHA256.fullmatch(asset.sha256):
            wrong(f"targets.{target}: {asset.sha256!r} is not a sha256")
        if "/" in asset.name or "\\" in asset.name or asset.name in ("", ".", ".."):
            wrong(f"targets.{target}: {asset.name!r} is not a file name")
    if not COMMIT.fullmatch(pin.commit):
        wrong(f"targets.{target}.commit is a full commit, not {pin.commit!r}")
    if pin.builder == "blaze" and (pin.llvm is None or pin.binaries is None or pin.speedup is None):
        wrong(f"targets.{target} is blaze's: it names its llvm, binaries and speedup")
    if pin.speedup is not None and (pin.speedup.plain_ms < 1 or pin.speedup.runs < 1):
        wrong(f"targets.{target}.speedup was measured: plain_ms and runs are at least 1")
    return pin


def files_of(pin: Pin) -> list[Asset]:
    """The files a pin names: the profile, the BOLT profile and the binaries."""
    assets = [Asset(pin.name, pin.sha256)]
    if pin.bolt is not None:
        assets.append(Asset(pin.bolt.name, pin.bolt.sha256))
    if pin.binaries is not None:
        assets.append(pin.binaries)
    return assets


def pin_json(pin: Pin) -> dict[str, Any]:
    return dataclasses.asdict(pin)


def dump_pins(pins: Pins) -> str:
    targets = {t: pin_json(p) for t, p in pins.targets}
    top = {"toolchain": pins.toolchain, "release": pins.release, "targets": targets}
    return json.dumps(top, indent=2) + "\n"


def merge_pins(pins: Pins, toolchain: str, entries: list[tuple[str, Pin]]) -> Pins:
    """`pins` with `entries` in. A new toolchain drops every other target's profile: a profile is
    of the code a toolchain built."""
    seen = [t for t, _ in entries]
    if len(set(seen)) != len(seen):
        raise Failed(f"two outputs are of one target: {sorted(seen)}")
    kept = dict(pins.targets) if pins.toolchain == toolchain else {}
    kept.update(entries)
    return Pins(toolchain, pins.release, tuple(sorted(kept.items())))


def release_of(url: str) -> tuple[str, str]:
    """The repository and tag of a release download URL."""
    found = RELEASE.fullmatch(url)
    if found is None:
        raise Failed(f"{url!r} is not a GitHub release download URL")
    return found.group(1), found.group(2)


def pack(files: list[Path], top: str, out: Path) -> None:
    """`files` as `top/<name>` in the tar.xz `out`, the same bytes for the same files."""

    def plain(info: tarfile.TarInfo) -> tarfile.TarInfo:
        info.mtime = 0
        info.uid = info.gid = 0
        info.uname = info.gname = ""
        info.mode = 0o755
        return info

    with tarfile.open(out, "w:xz", format=tarfile.PAX_FORMAT) as tar:
        for path in sorted(files, key=lambda p: p.name):
            tar.add(path, f"{top}/{path.name}", recursive=False, filter=plain)


def index_path(name: str) -> str:
    """Where the sparse index keeps the versions of crate `name`."""
    low = name.lower()
    if len(low) <= 2:
        return f"{len(low)}/{low}"
    if len(low) == 3:
        return f"3/{low[0]}/{low}"
    return f"{low[:2]}/{low[2:4]}/{low}"


def listed_cksum(index: str, name: str, version: str) -> str:
    """The sha256 the sparse index file `index` of crate `name` lists for `version`."""
    for line in index.splitlines():
        if not line.strip():
            continue
        entry = json.loads(line)
        if not isinstance(entry, dict) or entry.get("vers") != version:
            continue
        cksum = entry.get("cksum")
        if not isinstance(cksum, str) or not SHA256.fullmatch(cksum):
            raise Failed(f"{name} {version}: the index lists the checksum {cksum!r}")
        return cksum
    raise Failed(f"{name}: the index lists no version {version}")


def unpack(data: bytes, top: str, dest: Path) -> None:
    """A `.crate`'s `data`, every entry of which is under `top/`, unpacked as `dest`."""
    staging = dest.with_name(dest.name + ".unpack")
    shutil.rmtree(staging, ignore_errors=True)
    try:
        with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as tar:
            stray = [n for n in tar.getnames() if n != top and not n.startswith(top + "/")]
            if stray:
                raise Failed(f"{top}.crate has entries outside {top}/: {stray[:3]}")
            tar.extractall(staging, filter="data")
    except tarfile.TarError as e:
        raise Failed(f"unpacking {top}.crate: {e}") from None
    (staging / top).rename(dest)
    shutil.rmtree(staging)


# The steps.

type Env = dict[str, str]


def run(step: str, cmd: list[str], *, secs: float, cwd: Path = ROOT, env: Env | None = None) -> str:
    """`cmd`'s output, or Failed naming the step, the command, its exit and its output's tail. An
    `env` of None is this process's."""
    print(f"== {step}: {shlex.join(cmd)}", flush=True)
    try:
        done = subprocess.run(
            cmd, cwd=cwd, env=env, timeout=secs, stdout=subprocess.PIPE, stderr=subprocess.STDOUT
        )
    except subprocess.TimeoutExpired:
        raise Failed(f"{step}: {shlex.join(cmd)} ran over {secs:.0f}s") from None
    except OSError as e:
        raise Failed(f"{step}: {shlex.join(cmd)}: {e}") from None
    out = done.stdout.decode(errors="replace")
    if done.returncode != 0:
        tail = "\n".join(out.splitlines()[-40:])
        raise Failed(f"{step}: {shlex.join(cmd)} exited {done.returncode}:\n{tail}")
    return out


def download(url: str) -> bytes:
    try:
        with urllib.request.urlopen(url, timeout=FETCH_SECS) as answer:
            body: bytes = answer.read()
            return body
    except (urllib.error.URLError, OSError) as e:
        raise Failed(f"fetching {url}: {e}") from None


@dataclass(frozen=True)
class Llvm:
    prefix: Path
    commit: str

    def tool(self, name: str) -> Path:
        path = self.prefix / "bin" / name
        if not path.is_file():
            raise Failed(f"the pinned LLVM at {self.prefix} has no {name} (.github/scripts/llvm.py)")
        return path


@dataclass(frozen=True)
class Tree:
    """A project's pristine sources, trained on by `turns`."""

    name: str
    path: Path
    edit: str
    turns: tuple[Turn, ...]


def fetch_llvm(target: str, work: Path) -> Llvm:
    prefix = work / "llvm"
    cmd = [sys.executable, str(LLVM_PY), "fetch", "--target", target, "--dir", str(prefix)]
    run("fetch the pinned LLVM", cmd, secs=1800)
    commit = json.loads(LLVM_PIN.read_text())["commit"]
    return Llvm(prefix, commit)


def build_blaze(build: Build, llvm: Llvm, stage: str, flags: list[str], work: Path) -> Path:
    """blaze built by `builder` with `flags`; the directory of its binaries."""
    target_dir = work / f"target-{stage}"
    env = dict(os.environ)
    env.update(
        CARGO_TARGET_DIR=str(target_dir),
        CARGO_ENCODED_RUSTFLAGS=encoded_rustflags(flags),
        LLVM_SYS_231_PREFIX=str(llvm.prefix),
        BLAZE_HOME=str(work / f"home-build-{stage}"),
    )
    env.pop("RUSTFLAGS", None)
    cmd = [build.builder, "build", "--profile", PROFILE, "--target", build.target, "-p", PACKAGE]
    run(f"build blaze ({stage})", [*cmd, "--features", "llvm"], env=env, secs=build.build_secs)
    stop(Path(build.builder), env)
    built = target_dir / build.target / PROFILE
    bins = sorted(p.stem for p in (ENGINE / "src" / "bin").glob("*.rs"))
    dest = work / f"bin-{stage}"
    shutil.rmtree(dest, ignore_errors=True)
    dest.mkdir(parents=True)
    for name in binaries((ENGINE / "Cargo.toml").read_text(), bins):
        path = built / exe(build.target, name)
        if not path.is_file():
            raise Failed(f"build blaze ({stage}) made no {path}")
        shutil.copy2(path, dest / path.name)
    return dest


def exe(target: str, name: str) -> str:
    return name + (".exe" if "-windows" in target else "")


def stop(blaze: Path, env: dict[str, str]) -> None:
    run("stop the daemon", [str(blaze), "stop"], env=env, secs=120)


def wait_exit(bins: Path) -> None:
    """Every process of a binary in `bins` gone, so each wrote its profile. pgrep only: a process
    another lane runs is never signalled."""
    deadline = time.monotonic() + EXIT_SECS
    while True:
        pattern = re.escape(f"{bins}{os.sep}")
        alive = subprocess.run(["pgrep", "-f", pattern], stdout=subprocess.PIPE).stdout.split()
        if not alive:
            return
        if time.monotonic() > deadline:
            raise Failed(f"processes of {bins} still run {EXIT_SECS:.0f}s after `blaze stop`: {alive}")
        time.sleep(1)


def prepare(training: Training, work: Path) -> list[Tree]:
    """Each project's pristine sources, under `work/sources`."""
    sources = work / "sources"
    sources.mkdir(parents=True, exist_ok=True)
    out: list[Tree] = []
    for project in training.projects:
        dest = sources / project.name
        if not dest.is_dir():
            part = sources / f"{project.name}.part"
            shutil.rmtree(part, ignore_errors=True)
            part.mkdir()
            if project.source == "self":
                tar_path = sources / "self.tar"
                run("archive blaze", ["git", "archive", "-o", str(tar_path), "HEAD"], secs=600)
                with tarfile.open(tar_path) as tar:
                    tar.extractall(part, filter="data")
                tar_path.unlink()
            else:
                fetch(project, part)
            part.rename(dest)
        out.append(Tree(project.name, dest, project.edit, project.turns))
    if training.corpus is not None:
        out += prepare_corpus(training.corpus, sources / "corpus")
    for tree in out:
        if not (tree.path / tree.edit).is_file():
            raise Failed(f"{tree.name} has no edit file {tree.edit}")
    return out


def fetch(project: Project, dest: Path) -> None:
    """`project.repo` at `project.rev`, checked out into the empty `dest`."""
    step = f"fetch {project.name}"
    git = ["git", "-C", str(dest)]
    run(step, ["git", "init", "-q", str(dest)], secs=60)
    run(step, [*git, "fetch", "-q", "--depth", "1", project.repo, project.rev], secs=1800)
    run(step, [*git, "checkout", "-q", "FETCH_HEAD"], secs=600)
    got = run(step, [*git, "rev-parse", "HEAD"], secs=60).strip()
    if got != project.rev:
        raise Failed(f"{step}: {project.repo} gave {got}, not {project.rev}")


def prepare_corpus(corpus: Corpus, sources: Path) -> list[Tree]:
    listed = of(CorpusFile, typed_input.read(str(ROOT / corpus.path)), corpus.path)
    by_name = {c.name: c for c in listed.crates}
    if missing := sorted(set(corpus.crates) - set(by_name)):
        raise Failed(f"{corpus.path} has no crates {missing}")
    chosen = [by_name[n] for n in corpus.crates] if corpus.crates else list(listed.crates)
    sources.mkdir(parents=True, exist_ok=True)
    out: list[Tree] = []
    for crate in chosen:
        top = f"{crate.name}-{crate.version}"
        dest = sources / top
        if not dest.is_dir():
            index = download(INDEX + index_path(crate.name)).decode()
            cksum = listed_cksum(index, crate.name, crate.version)
            data = download(DOWNLOAD.format(name=crate.name, version=crate.version))
            got = hashlib.sha256(data).hexdigest()
            if got != cksum:
                raise Failed(f"{top}.crate: its sha256 is {got}, and the index lists {cksum}")
            unpack(data, top, dest)
        out.append(Tree(f"corpus/{top}", dest, corpus.edit, corpus.turns))
    return out


def on_path(bins: Path, env: dict[str, str]) -> str:
    return f"{bins}{os.pathsep}{env.get('PATH', '')}"


def train(build: Build, trees: list[Tree], bins: Path, stage: str, work: Path, extra: dict[str, str]) -> float:
    """Every turn of every tree run by the blaze in `bins`, on a fresh copy of the sources and a
    fresh BLAZE_HOME; then its daemon stopped and gone. The seconds the turns took, copies and
    stops apart."""
    root = work / f"train-{stage}"
    shutil.rmtree(root, ignore_errors=True)
    root.mkdir(parents=True)
    env = dict(os.environ)
    env.update(extra)
    env.update(BLAZE_HOME=str(root / "home"), PATH=on_path(bins, env))
    blaze = bins / exe(build.target, "blaze")
    spent = 0.0
    try:
        for tree in trees:
            copy = root / tree.name
            shutil.copytree(tree.path, copy, symlinks=True)
            edits = 0
            for i, turn in enumerate(tree.turns):
                if turn.edit:
                    edits += 1
                    with (copy / tree.edit).open("a") as f:
                        f.write(edit_text(edits))
                step = f"train {stage}: {tree.name} turn {i}"
                began = time.monotonic()
                run(step, [str(blaze), *turn.args], cwd=copy, env=env, secs=build.turn_secs)
                spent += time.monotonic() - began
    except Failed:
        try:
            stop(blaze, env)
        except Failed as e:
            print(f"after the failure below, {e}", file=sys.stderr, flush=True)
        raise
    stop(blaze, env)
    wait_exit(bins)
    return spent


def rotated[T](items: list[T], by: int) -> list[T]:
    """`items` from its `by`th on, then the rest: round `by` of a measurement runs the sets in
    this order, so none always runs first on a cold machine."""
    at = by % len(items)
    return items[at:] + items[:at]


def measure(build: Build, trees: list[Tree], sets: dict[str, Path], work: Path) -> dict[str, float]:
    """Each set of binaries' least seconds over `measure_runs` rounds of `trees`' turns, each round
    running every set, in a rotated order."""
    least = {name: math.inf for name in sets}
    for i in range(build.measure_runs):
        for name in rotated(sorted(sets), i):
            least[name] = min(least[name], train(build, trees, sets[name], f"measure-{name}", work, {}))
    return least


def saved(plain: float, optimised: float) -> int:
    """The thousandths of `plain` that `optimised` saved, rounded down; under 0 when slower."""
    return math.floor(1000 * (plain - optimised) / plain)


def speedup_of(least: dict[str, float], runs: int, floor: int) -> Speedup:
    """The speedup of the least times of the sets `plain`, `pgo` and, where BOLT ran, `total`;
    Failed when the optimised blaze saves under `floor` thousandths."""
    plain = least["plain"]
    if not plain > 0:
        raise Failed("the measured turns took no time with the plain blaze")
    pgo = saved(plain, least["pgo"])
    total = saved(plain, least.get("total", least["pgo"]))
    got = Speedup(max(1, round(plain * 1000)), pgo, total, runs)
    if total < floor:
        raise Failed(
            f"the optimised blaze saves {total}/1000 of a plain build's {got.plain_ms} ms on the measured "
            f"turns ({pgo}/1000 by the profile alone), under the {floor}/1000 a profile that applies saves"
        )
    return got


def plain_bins(build: Build, llvm: Llvm, work: Path) -> Path:
    """The speedup's baseline: `baseline`, checked, or this commit built here with no profile."""
    if not build.baseline:
        return build_blaze(build, llvm, "plain", [], work)
    bins = Path(build.baseline).resolve()
    for name in ("blaze", "blaze-engine"):
        if not (bins / exe(build.target, name)).is_file():
            raise Failed(f"baseline {bins} has no {exe(build.target, name)}")
    return bins


def merge_profdata(llvm: Llvm, raw: Path, out: Path, min_count: int) -> int:
    raws = sorted(str(p) for p in raw.glob("*.profraw"))
    if not raws:
        raise Failed(f"training wrote no raw profile into {raw}")
    profdata = str(llvm.tool("llvm-profdata"))
    run("merge the profiles", [profdata, "merge", "-o", str(out), *raws], secs=1800)
    count = profile_count(run("count the profile", [profdata, "show", str(out)], secs=600))
    if count < min_count:
        raise Failed(f"the profile has {count} counts, under the {min_count} a trained blaze makes")
    return count


def bolt(build: Build, llvm: Llvm, trees: list[Tree], bins: Path, work: Path) -> tuple[Path, Path]:
    """The binaries of `bins` with `bolt_bins` optimised by BOLT, and the merged BOLT profile."""
    data = work / "bolt"
    shutil.rmtree(data, ignore_errors=True)
    data.mkdir()
    merged = work / "bolt.fdata"
    names = [exe(build.target, n) for n in build.bolt_bins]
    for name in names:
        if not (bins / name).is_file():
            raise Failed(f"bolt_bins names {name}, which blaze does not build")
    if build.bolt == "instrument":
        trained = work / "bin-bolt-instrumented"
        shutil.rmtree(trained, ignore_errors=True)
        shutil.copytree(bins, trained)
        for name in names:
            out = [f"--instrumentation-file={data / name}.fdata", "--instrumentation-file-append-pid"]
            cmd = [str(llvm.tool("llvm-bolt")), str(bins / name), "-instrument", "-o", str(trained / name)]
            run(f"instrument {name} for BOLT", [*cmd, *out], secs=3600)
        train(build, trees, trained, "bolt", work, {})
        parts = sorted(str(p) for p in data.glob("*.fdata"))
        if not parts:
            raise Failed(f"BOLT training wrote no profile into {data}")
    else:
        parts = perf_profiles(build, llvm, trees, bins, work, names, data)
    merged.write_text(run("merge the BOLT profiles", [str(llvm.tool("merge-fdata")), *parts], secs=1800))
    optimised = work / "bin-bolt"
    shutil.rmtree(optimised, ignore_errors=True)
    shutil.copytree(bins, optimised)
    layout = ["-reorder-blocks=ext-tsp", "-reorder-functions=cdsort", "-split-functions", "-split-all-cold"]
    layout += ["-split-eh", "-icf=1", "-use-gnu-stack", "-dyno-stats"]
    for name in names:
        cmd = [str(llvm.tool("llvm-bolt")), str(bins / name), "-o", str(optimised / name), f"-data={merged}"]
        run(f"optimise {name} with BOLT", [*cmd, *layout], secs=3600)
    return optimised, merged


def perf_profiles(
    build: Build, llvm: Llvm, trees: list[Tree], bins: Path, work: Path, names: list[str], data: Path
) -> list[str]:
    """BOLT profiles of `names` from `perf record` with branch stacks over a training run."""
    perf_data = work / "perf.data"
    record = subprocess.Popen(["perf", "record", "-a", "-e", "cycles:u", "-j", "any,u", "-o", str(perf_data)])
    try:
        train(build, trees, bins, "bolt", work, {})
    finally:
        record.send_signal(signal.SIGINT)
        try:
            code = record.wait(timeout=600)
        except subprocess.TimeoutExpired:
            raise Failed("perf record did not stop within 600s of SIGINT") from None
    if code != 0:
        raise Failed(f"perf record exited {code}: without branch stacks, train BOLT with bolt: instrument")
    parts: list[str] = []
    for name in names:
        part = data / f"{name}.fdata"
        cmd = [str(llvm.tool("perf2bolt")), "-p", str(perf_data), "-o", str(part), str(bins / name)]
        run(f"read perf for {name}", cmd, secs=3600)
        parts.append(str(part))
    return parts


def gate(build: Build, bins: Path) -> None:
    env = dict(os.environ)
    env.update(BLAZE_BIN=str(bins / exe(build.target, "blaze-engine")), PATH=on_path(bins, env))
    run("the optimised blaze runs", [str(bins / exe(build.target, "blaze")), "--version"], env=env, secs=60)
    for i, cmd in enumerate(build.gate):
        run(f"gate {i}", list(cmd), env=env, secs=build.build_secs)


def do_build(build: Build) -> None:
    out = Path(build.out).resolve()
    if out.exists() and any(out.iterdir()):
        raise Failed(f"{out} is not empty")
    work = Path(build.work).resolve() if build.work else out.with_name(out.name + ".work")
    work.mkdir(parents=True, exist_ok=True)
    training = parse_training(str(ROOT / build.training))
    toolchain = TOOLCHAIN.read_text().strip()
    commit = run("the commit", ["git", "rev-parse", "HEAD"], secs=60).strip()
    llvm = fetch_llvm(build.target, work)
    for tool in ("llvm-profdata", *(BOLT_TOOLS if build.bolt != "none" else ())):
        llvm.tool(tool)
    trees = prepare(training, work)
    timed = [tree for tree in trees if tree.name in training.measure]
    plain = plain_bins(build, llvm, work)

    raw = work / "raw"
    shutil.rmtree(raw, ignore_errors=True)
    raw.mkdir()
    instrumented = build_blaze(build, llvm, "generate", stage_flags(build.target, "generate", raw), work)
    train(build, trees, instrumented, "pgo", work, {"LLVM_PROFILE_FILE": str(raw / "%p-%m.profraw")})
    merged = work / "blaze.profdata"
    count = merge_profdata(llvm, raw, merged, build.min_count)

    optimised = build_blaze(build, llvm, "use", stage_flags(build.target, "use", merged), work)
    sets = {"plain": plain, "pgo": optimised}
    bolt_pin: Bolt | None = None
    bolt_data: Path | None = None
    if build.bolt != "none":
        optimised, bolt_data = bolt(build, llvm, trees, optimised, work)
        sets["total"] = optimised
    gate(build, optimised)
    speedup = speedup_of(measure(build, timed, sets, work), build.measure_runs, build.min_speedup)

    out.mkdir(parents=True, exist_ok=True)
    stem = f"blaze-{toolchain}-{build.target}"
    shutil.copy2(merged, out / "profile")
    profile = named(out / "profile", stem, ".profdata")
    if bolt_data is not None and build.bolt != "none":
        shutil.copy2(bolt_data, out / "bolt")
        asset = named(out / "bolt", stem, ".fdata")
        bolt_pin = Bolt(build.bolt, asset.name, asset.sha256)
    pack(sorted(optimised.iterdir()), f"blaze-pgo-{build.target}", out / "binaries")
    archive = named(out / "binaries", f"blaze-pgo-{toolchain}-{build.target}", ".tar.xz")
    pin = Pin(profile.name, profile.sha256, commit, count, None, "blaze", llvm.commit, bolt_pin, archive, speedup)
    entry = {"target": build.target, "toolchain": toolchain, "pin": pin_json(pin)}
    (out / f"{build.target}.json").write_text(json.dumps(entry, indent=2) + "\n")
    print(
        f"{build.target}: {profile.name}, {count} counts, binaries {archive.name}, "
        f"{speedup.total}/1000 faster than plain ({speedup.pgo}/1000 by the profile)",
        flush=True,
    )


def read_output(path: Path) -> tuple[str, str, Pin]:
    """A `<target>.json` of a build: its target, toolchain and pin, each file it names checked."""
    value = json.loads(path.read_text())
    if not isinstance(value, dict) or sorted(value) != ["pin", "target", "toolchain"]:
        raise Failed(f"{path} has the fields pin, target, toolchain")
    target, toolchain = value["target"], value["toolchain"]
    if not isinstance(target, str) or not isinstance(toolchain, str) or path.name != f"{target}.json":
        raise Failed(f"{path}: target and toolchain are strings, and the file is named for its target")
    try:
        pin = read_pin(target, value["pin"])
    except typed_input.InputError as e:
        raise Failed(f"{path}: {e}") from None
    if pin.builder != "blaze":
        raise Failed(f"{path}: a build's pin is blaze's")
    for asset in files_of(pin):
        file = path.parent / asset.name
        got = sha256_of(file) if file.is_file() else "nothing"
        if got != asset.sha256:
            raise Failed(f"{file}: its sha256 is {got}, and {path.name} names {asset.sha256}")
    return target, toolchain, pin


def do_publish(publish: Publish) -> None:
    if not publish.dirs:
        raise Failed("dirs is empty")
    toolchain = TOOLCHAIN.read_text().strip()
    try:
        pins = load_pins(PINS.read_text())
    except typed_input.InputError as e:
        raise Failed(f"{PINS}: {e}") from None
    entries: list[tuple[str, Pin]] = []
    uploads: list[Path] = []
    for d in publish.dirs:
        outputs = sorted(Path(d).glob("*.json"))
        if not outputs:
            raise Failed(f"{d} has no <target>.json")
        for path in outputs:
            target, built_by, pin = read_output(path)
            if built_by != toolchain:
                raise Failed(f"{path} is of {built_by}, and harness/toolchain pins {toolchain}")
            entries.append((target, pin))
            uploads += [path.parent / a.name for a in files_of(pin)]
    merged = merge_pins(pins, toolchain, entries)
    if publish.upload:
        repo, tag = release_of(pins.release)
        cmd = ["gh", "release", "upload", tag, "--repo", repo, "--clobber", *map(str, uploads)]
        run("upload the profiles", cmd, secs=3600)
    PINS.write_text(dump_pins(merged))
    print(f"{PINS.relative_to(ROOT)} pins {', '.join(t for t, _ in entries)}", flush=True)


def main() -> None:
    if len(sys.argv) != 2:
        fail("takes one JSON input: inline, a file's path or -")
    try:
        given = parse_input(sys.argv[1])
    except (typed_input.InputError, OSError) as e:
        fail(str(e))
    try:
        if isinstance(given, Build):
            do_build(given)
        else:
            do_publish(given)
    except (Failed, typed_input.InputError) as e:
        fail(str(e))


if __name__ == "__main__":
    main()
