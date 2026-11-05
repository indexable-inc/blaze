#!/usr/bin/env python3
"""What blaze fetches, put into the R2 bucket `blaze-releases` (docs/release.md, "Where the bytes are").

The bucket is served at https://blaze-releases.ix.dev, the one place blaze fetches a release, the
library's source and a pinned native input from (GitHub's copies are no fallback). Its keys:

    v<version>/<asset>      each asset of a release: blaze-v<version>-<triple>.tar.xz and .tar.gz (+ .sha256),
                            SHA256SUMS, B3SUMS, library-<digest>.tar.xz      immutable
    channels/<channel>      the version `stable` or `canary` names, and a newline     60 s
    latest.json             what every daemon checks (promote.py latest)              60 s
    native/<name>           a pinned native input's archive (llvm.json's targets), its
                            name ending in its sha256's first 16 hex digits           immutable

A version is one release for good and an archive's name carries its hash, so those keys never
change bytes and are cached for a year at Cloudflare's edge and by clients; a fetch still checks
every byte against the pinned hash (versions.rs, library.rs, serve/native.rs, llvm.py).

    r2.py '{"kind": "release", "tag": "v0.20261105.1", "dir": "out"}'
    r2.py '{"kind": "native", "dir": "llvm"}'          # every *.tar.xz under dir
    r2.py '{"kind": "channels", "dir": "channels"}'    # the files stable and canary in dir
    r2.py '{..., "run": true}'                         # put them with the wrangler on PATH

Without `run` it prints one wrangler command per line (without the leading `wrangler`), which
the workflows hand to cloudflare/wrangler-action, so the R2 token reaches wrangler alone and the
key layout lives here alone. With `run` (backfilling by hand, logged in with `wrangler login`)
it puts them, four at a time, each bounded; a failure names the object, the command, its exit
code and its stderr's tail. The JSON is inline, a file's path, or `-` for stdin. Stdlib only,
Python 3.11+.
"""

import asyncio
import json
import re
import shlex
import sys
import typing
from dataclasses import dataclass, fields
from pathlib import Path

BUCKET = "blaze-releases"
IMMUTABLE = "public,max-age=31536000,immutable"
FRESH = "public,max-age=60"
TAG = re.compile(r"v\d+\.\d+\.\d+")
# What a release publishes (release.yaml's `publish`), by name: nothing else in its directory is put.
RELEASE_ASSET = re.compile(r"blaze-v\d+\.\d+\.\d+-[a-z0-9_]+-[a-z0-9_-]+\.tar\.(gz|xz)(\.sha256)?|SHA256SUMS|B3SUMS|library-[0-9a-f]{64}\.tar\.xz")
# A native archive's name ends in its sha256's first 16 hex digits (llvm.py archive_stem).
NATIVE = re.compile(r"[A-Za-z0-9._-]+-[0-9a-f]{16}\.tar\.xz")
CHANNELS = ("stable", "canary")
# versions.rs's B3SUMS_SINCE: the first date of releases that all publish B3SUMS.
B3SUMS_SINCE = 20261031


class Failed(Exception):
    """A put that failed, its message naming the object, the command and what it said."""


@dataclass(frozen=True)
class Input:
    kind: typing.Literal["release", "native", "channels"]
    dir: str
    tag: str = ""
    run: bool = False


@dataclass(frozen=True)
class Put:
    key: str
    file: Path
    cache: str
    type: str

    def command(self) -> list[str]:
        return ["r2", "object", "put", f"{BUCKET}/{self.key}", f"--file={self.file}",
                f"--content-type={self.type}", f"--cache-control={self.cache}", "--remote"]


def typed(text: str) -> Input:
    """The input from JSON `text` (inline, a path, or `-`); exits naming the field that is wrong."""
    if text == "-":
        text = sys.stdin.read()
    elif not text.lstrip().startswith("{"):
        text = Path(text).read_text()
    try:
        value = json.loads(text)
    except json.JSONDecodeError as e:
        sys.exit(f"r2.py: the input is not JSON: {e}")
    if not isinstance(value, dict):
        sys.exit("r2.py: the input is a JSON object")
    known = {f.name for f in fields(Input)}
    if unknown := sorted(set(value) - known):
        sys.exit(f"r2.py: no field {', '.join(unknown)} (fields: {', '.join(sorted(known))})")
    hints = typing.get_type_hints(Input)
    for name, given in value.items():
        hint = hints[name]
        if typing.get_origin(hint) is typing.Literal:
            if given not in typing.get_args(hint):
                sys.exit(f"r2.py: field {name} is one of {', '.join(map(repr, typing.get_args(hint)))}, not {given!r}")
        elif not isinstance(given, hint):
            sys.exit(f"r2.py: field {name} is a {hint.__name__}, not {given!r}")
    try:
        return Input(**value)
    except TypeError as e:
        sys.exit(f"r2.py: {e}")


def content_type(name: str) -> str:
    if name.endswith(".tar.gz"):
        return "application/gzip"
    if name.endswith(".tar.xz"):
        return "application/x-xz"
    return "text/plain"


def planned(it: Input) -> list[Put]:
    """The objects `it` puts, sorted by key; exits naming what is missing or not one of them."""
    root = Path(it.dir)
    if not root.is_dir():
        sys.exit(f"r2.py: {root} is no directory")
    if it.kind == "release":
        if not TAG.fullmatch(it.tag):
            sys.exit(f"r2.py: a release's tag is v0.YYYYMMDD.N, not {it.tag!r}")
        files = sorted(p for p in root.iterdir() if p.is_file() and RELEASE_ASSET.fullmatch(p.name))
        names = {p.name for p in files}
        # Every release publishes SHA256SUMS, and those dated B3SUMS_SINCE or later B3SUMS too
        # (versions.rs refuses one of them without it).
        sums = ("SHA256SUMS", "B3SUMS") if int(it.tag.split(".")[1]) >= B3SUMS_SINCE else ("SHA256SUMS",)
        if missing := [n for n in sums if n not in names]:
            sys.exit(f"r2.py: {root} has no {' or '.join(missing)} of {it.tag}")
        if wrong := [n for n in names if n.startswith("blaze-") and not n.startswith(f"blaze-{it.tag}-")]:
            sys.exit(f"r2.py: {root} has {', '.join(sorted(wrong))}, which are not {it.tag}'s")
        puts = [Put(f"{it.tag}/{p.name}", p, IMMUTABLE, content_type(p.name)) for p in files]
    elif it.kind == "native":
        files = sorted(root.rglob("*.tar.xz"))
        if not files:
            sys.exit(f"r2.py: {root} has no native archive (*.tar.xz)")
        if wrong := [p.name for p in files if not NATIVE.fullmatch(p.name)]:
            sys.exit(f"r2.py: {', '.join(wrong)} do not end in their sha256's 16 hex digits, so they are no native archive")
        puts = [Put(f"native/{p.name}", p, IMMUTABLE, content_type(p.name)) for p in files]
    else:
        puts = []
        for channel in CHANNELS:
            p = root / channel
            if not p.is_file() or not re.fullmatch(r"\d+\.\d+\.\d+\n", p.read_text()):
                sys.exit(f"r2.py: {p} is not the version the {channel} channel names and a newline")
            puts.append(Put(f"channels/{channel}", p, FRESH, content_type(channel)))
    keys = [p.key for p in puts]
    if len(set(keys)) != len(keys):
        sys.exit(f"r2.py: two files for one key among {keys}")
    return sorted(puts, key=lambda p: p.key)


async def put(one: Put, gate: asyncio.Semaphore) -> None:
    cmd = ["wrangler", *one.command()]
    async with gate:
        print(f"r2.py: {one.key} ({one.file.stat().st_size} bytes)", flush=True)
        proc = await asyncio.create_subprocess_exec(*cmd, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        try:
            async with asyncio.timeout(1800):
                out, err = await proc.communicate()
        except TimeoutError:
            proc.kill()
            raise Failed(f"r2.py: putting {one.key}: `{shlex.join(cmd)}` took over 30 minutes") from None
    if proc.returncode:
        tail = (err or out).decode(errors="replace")[-2000:]
        raise Failed(f"r2.py: putting {one.key}: `{shlex.join(cmd)}` exited {proc.returncode}:\n{tail}")


async def run(puts: list[Put]) -> None:
    gate = asyncio.Semaphore(4)
    async with asyncio.TaskGroup() as group:
        for one in puts:
            group.create_task(put(one, gate))


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    it = typed(sys.argv[1])
    puts = planned(it)
    if it.run:
        try:
            asyncio.run(run(puts))
        except* Failed as failed:
            sys.exit("\n".join(str(e) for e in failed.exceptions))
        print(f"r2.py: put {len(puts)} objects into {BUCKET}")
    else:
        for one in puts:
            print(shlex.join(one.command()))


if __name__ == "__main__":
    main()
