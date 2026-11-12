#!/usr/bin/env python3
"""diskbytes.py: every byte blaze leaves on disk after one scripted session, and what the edits cost.

    diskbytes.py run '<json>' | FILE | -      a fresh home, a build, the edits, then the tables
    diskbytes.py home '<json>' | FILE | -     the tables of a home that is there, no session
    diskbytes.py tree '<json>' | FILE | -     the tables of a directory that is there (Relax's)

`run` makes an empty home and an empty machine directory under `work`, a hello-world beside
them, builds and runs it (`blaze run -q`), makes `body_edits` body edits and `comment_edits`
comment edits (each built and run, each answer checked), waits `settle_seconds` for what the
daemon does after its last answer, stops the daemon, and then reads what is on disk:

  by directory   each directory of the home (and the machine directory): files, their lengths,
                 the blocks they have, and the blocks that are theirs alone. Two files that share
                 blocks (a clone, a reflink) are counted once: on Linux by the union of the
                 extents FIEMAP gives, on macOS by `F_LOG2PHYS_EXT`'s device offsets.
  by kind        the store's records by kind and form, read from the packs as blaze-cas lays
                 them out (crates/blaze-cas/src/disk.rs: a 16-byte pack header, 96-byte record
                 headers, 16-byte alignment), its whole files by the kind of the record that
                 names them, and every other file by what it is (the daemon image, the library,
                 the build directory's outputs, the profile history, the journal, ...).
  duplicates     per kind, the bytes whose content is also somewhere else: a record whose
                 content another record holds under another key, a file whose bytes another
                 file or a record holds (compared by a hash of the whole value), and files that
                 are another's clone (no blocks of their own).
  compressible   per kind, its values through zstd at two levels: all of them as one stream
                 (what an archive of the kind would be), each alone (what a record costs when
                 it is compressed alone), and each alone with a dictionary trained on the kind
                 (what the store's cold tier can reach and still read one record).

The edit loop's rows are the wall time of `blaze run -q` after each edit, by kind of edit, with
the median; a run whose answer or exit status is wrong is counted as bad and fails the script.

The answers are one JSON object written to `out` and the tables printed. Nothing here reads
`~/.cache/blaze` or writes outside `work`: the session's XDG_CACHE_HOME is under `work`.
"""

from __future__ import annotations

import fcntl
import hashlib
import json
import os
import platform
import re
import shutil
import statistics
import struct
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "tools", "typed-input", "src"))
from typed_input import fail, parse  # noqa: E402

SCRIPT = "diskbytes.py"
LEVELS = (3, 19)
KINDS = ("query", "code", "debug", "run", "output", "image", "machine", "derived")
FORMS = ("raw", "zstd", "chunked", "external", "dictionary", "lz4", "warm", "cold-columns")
PACK_HEADER, RECORD_HEADER, ALIGN, BLOCK = 16, 96, 16, 4096
# A kind with fewer values than this gets no dictionary: zstd refuses to train on so few.
DICTIONARY_FROM = 16
# zstd's trainer reads at most 2 GiB of samples and wants many; this bounds what is written out.
SAMPLES_MOST = 20_000


@dataclass(frozen=True)
class Run:
    # The directory with `blaze` and `blaze-engine`.
    bin: str
    # What was built, for the report: the commit and whatever else names the tree.
    commit: str
    # An empty or missing directory on a path with no symlink in it; emptied first.
    work: str
    # Where the answers go, as JSON.
    out: str
    # The zstd binary, absolute.
    zstd: str
    machine: str = ""
    body_edits: int = 5
    comment_edits: int = 5
    settle_seconds: int = 8
    # Between two turns: an edit made while the daemon still works on the last (what it does
    # after an answer) waits for it, and is no measure of an edit.
    pause_seconds: float = 1.5
    # Keep the daemon running (and the home as it is while it runs) instead of stopping it.
    stop: bool = True


@dataclass(frozen=True)
class Home:
    # A home that is there, and the machine directory its daemon shared (`<XDG_CACHE_HOME>/blaze`).
    home: str
    machine_dir: str
    out: str
    zstd: str
    label: str = ""


@dataclass(frozen=True)
class Tree:
    # The directory to read (Relax's `target`).
    dir: str
    out: str
    zstd: str
    label: str = ""


# ---------------------------------------------------------------------------------------------
# Blocks: which bytes of the disk a file has.


def extents_linux(fd: int, size: int) -> list[tuple[int, int]]:
    """The physical extents of a file, by FIEMAP: (device offset, length)."""
    FS_IOC_FIEMAP, COUNT = 0xC020660B, 512
    out: list[tuple[int, int]] = []
    start = 0
    while True:
        buf = bytearray(struct.pack("=QQIIII", start, (1 << 64) - 1 - start, 1, 0, COUNT, 0) + bytes(56 * COUNT))
        fcntl.ioctl(fd, FS_IOC_FIEMAP, buf)
        mapped = struct.unpack_from("=I", buf, 20)[0]
        last = False
        for i in range(mapped):
            logical, physical, length, _, _, flags = struct.unpack_from("=QQQQQI", buf, 32 + 56 * i)
            # Inline data (0x200) and not-yet-allocated delayed extents (0x4) have no blocks.
            if not flags & (0x200 | 0x4):
                out.append((physical, length))
            start = logical + length
            last = bool(flags & 0x1)
        if mapped == 0 or last:
            return out


def extents_macos(fd: int, size: int) -> list[tuple[int, int]]:
    """The physical extents of a file: (device offset, length). `F_LOG2PHYS_EXT` gives the device
    offset of a file offset, but answers for a hole as if it had blocks, so the holes are found
    first (`SEEK_DATA`, `SEEK_HOLE`) and only the data between them is asked about."""
    F_LOG2PHYS_EXT = 65
    out: list[tuple[int, int]] = []
    at = 0
    while at < size:
        try:
            data = os.lseek(fd, at, os.SEEK_DATA)
        except OSError as e:
            # ENXIO: nothing but a hole from here to the end.
            if e.errno == 6:
                break
            raise
        hole = os.lseek(fd, data, os.SEEK_HOLE)
        while data < hole:
            # struct log2phys, packed to 4: flags, then the bytes asked about, then the offset.
            answer = fcntl.fcntl(fd, F_LOG2PHYS_EXT, struct.pack("=Iqq", 0, hole - data, data))
            _, contiguous, device = struct.unpack("=Iqq", answer)
            if contiguous <= 0:
                raise OSError(0, f"F_LOG2PHYS_EXT at {data} gave no bytes")
            length = min(contiguous, hole - data)
            out.append((device, length))
            data += length
        at = hole
    return out


def extents(path: str, size: int) -> list[tuple[int, int]]:
    if size == 0:
        return []
    fd = os.open(path, os.O_RDONLY)
    try:
        return extents_linux(fd, size) if sys.platform == "linux" else extents_macos(fd, size)
    finally:
        os.close(fd)


def union(spans: list[tuple[int, int]]) -> int:
    """The bytes covered by `spans`, each counted once."""
    total, end = 0, -1
    for start, length in sorted(spans):
        if start + length <= end:
            continue
        total += start + length - max(start, end)
        end = start + length
    return total


# ---------------------------------------------------------------------------------------------
# What a file is.


def classify(rel: str) -> str:
    """The kind of a file of a home by its path (the store's are refined from its records)."""
    parts = rel.split("/")
    name = parts[-1]
    if parts[0] == "v2":
        if len(parts) > 2 and parts[1] == "store":
            if name.startswith("pack-"):
                return "store pack"
            if name.startswith("file-"):
                return "store whole file"
            if name.startswith("checks-"):
                return "store block checks"
            if name.startswith("index-") or name == "head":
                return "store index and head"
            if parts[2] == "placed":
                return "store placed (made files)"
            return "store other"
        if len(parts) > 2 and parts[1] == "db":
            return "daemon image"
        if len(parts) > 2 and parts[1] == "profile":
            if name.endswith(".parquet"):
                return "profile history: " + name.split(".")[-2]
            if name.startswith("flight-"):
                return "profile flight recorder"
            if name == "why.ledger":
                return "profile why ledger"
            return "profile other"
        if parts[1] == "journal" or (len(parts) > 2 and parts[1] == "journal"):
            return "journal"
        if parts[1] == "gc-ledger.json":
            return "gc ledger"
        if parts[1].startswith("serve.log"):
            return "daemon log"
        if len(parts) > 2 and parts[1] == "vfs":
            return "vfs state"
        return "v2 other: " + parts[1]
    if parts[0] == "library":
        return "library copy"
    if parts[0] == "build":
        ext = os.path.splitext(name)[1] or "(none)"
        if ".dSYM/" in rel:
            return "build dir: dSYM"
        if ext in (".rlib", ".rmeta", ".o", ".d", ".json", ".a", ".so", ".dylib", ".dwo", ".dwp"):
            return "build dir: " + ext
        return "build dir: other"
    if parts[0] == "machine":
        ext = os.path.splitext(name)[1] or "(none)"
        return "machine std: " + (ext if len(ext) < 8 else "other")
    if parts[0] == "sources":
        return "sources"
    return "other: " + parts[0]


def digest(data: bytes) -> bytes:
    return hashlib.blake2b(data, digest_size=16).digest()


def file_digest(path: str) -> bytes:
    h = hashlib.blake2b(digest_size=16)
    with open(path, "rb") as f:
        while chunk := f.read(1 << 20):
            h.update(chunk)
    return h.digest()


@dataclass
class Value:
    """One value on disk: a file, or a record of a pack."""

    kind: str
    where: str
    # Its bytes as kept (a record's span in its pack; a file's length).
    stored: int
    # The blocks that are counted to it once every sharing is taken out (a record: its span).
    own: int
    digest: bytes
    # For a record: the pack's path and the payload's place, to read it back.
    path: str
    offset: int
    length: int
    # A record whose content another holds, or a file with no blocks of its own, or equal bytes.
    duplicate_of: str = ""


def pack_records(path: str, rel: str) -> tuple[list[Value], dict[str, int], int]:
    """The records of one pack, the content each whole file was named by, and its padding."""
    values: list[Value] = []
    externals: dict[str, int] = {}
    padding = 0
    with open(path, "rb") as f:
        data = f.read()
    if data[:7] != b"BLZPACK":
        fail(SCRIPT, f"{path} does not start as a pack does")
    padding += PACK_HEADER
    at = PACK_HEADER
    while at + RECORD_HEADER <= len(data):
        if data[at : at + 4] != b"BLZR":
            # The rest of a commit's last block is zeros; the next record starts on a block.
            nxt = (at // BLOCK + 1) * BLOCK
            if any(data[at:nxt]):
                fail(SCRIPT, f"{path} at {at}: neither a record nor a commit's padding")
            padding += min(nxt, len(data)) - at
            at = nxt
            continue
        kind, form = data[at + 4], data[at + 5]
        stored, raw = struct.unpack_from("<QQ", data, at + 16)
        content = data[at + 64 : at + 96]
        span = (RECORD_HEADER + stored + ALIGN - 1) // ALIGN * ALIGN
        if kind >= len(KINDS) or form >= len(FORMS) or at + span > len(data):
            fail(SCRIPT, f"{path} at {at}: a record of no kind or form, or longer than its pack")
        label = f"store record: {KINDS[kind]} ({FORMS[form]})"
        if FORMS[form] == "external":
            externals[content.hex()] = kind
        values.append(Value(label, f"{rel}@{at}", span, span, content, path, at + RECORD_HEADER, stored))
        at += span
    padding += len(data) - at if at < len(data) else 0
    return values, externals, padding


def read_value(value: Value) -> bytes:
    with open(value.path, "rb") as f:
        f.seek(value.offset)
        return f.read(value.length)


def walk(root: str, top: str = "", running: bool = False) -> list[tuple[str, str, os.stat_result]]:
    """Every regular file under `root`: (path, path from `root` with `top` before it, its stat).
    `running`: a daemon is writing there, so a file listed may be gone when it is looked at."""
    out = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        for name in sorted(filenames):
            path = os.path.join(dirpath, name)
            try:
                st = os.lstat(path)
            except FileNotFoundError:
                if running:
                    continue
                raise
            if not os.path.isfile(path) or os.path.islink(path):
                continue
            rel = os.path.relpath(path, root)
            out.append((path, os.path.join(top, rel) if top else rel, st))
    return out


def measure(roots: list[tuple[str, str]], home_like: bool, zstd: str) -> dict:
    """The tables of the files under `roots` (each a directory and the name it is listed under)."""
    files = [it for root, top in roots for it in walk(root, top)]
    seen_inodes: dict[tuple[int, int], str] = {}
    all_extents: list[tuple[int, int]] = []
    by_dir: dict[str, dict[str, int]] = {}
    dir_extents: dict[str, list[tuple[int, int]]] = {}
    values: list[Value] = []
    externals: dict[str, int] = {}
    file_values: dict[str, Value] = {}
    padding = 0
    claimed: list[tuple[int, int]] = []

    def directory_of(rel: str) -> str:
        parts = rel.split("/")
        depth = 2 if parts[0] in ("v2", "build", "machine") else 1
        return "/".join(parts[:depth]) if len(parts) > depth else "/".join(parts[:-1]) or "."

    for path, rel, st in files:
        inode = (st.st_dev, st.st_ino)
        linked = seen_inodes.get(inode)
        ext = [] if linked else extents(path, st.st_size)
        seen_inodes.setdefault(inode, rel)
        # What of this file's blocks no earlier file had: its own, in the order files are walked.
        before = union(claimed)
        claimed.extend(ext)
        claimed = merged(claimed)
        own = union(claimed) - before
        d = directory_of(rel)
        row = by_dir.setdefault(d, {"files": 0, "length": 0, "blocks": 0})
        row["files"] += 1
        row["length"] += st.st_size
        row["blocks"] += 0 if linked else st.st_blocks * 512
        dir_extents.setdefault(d, []).extend(ext)
        all_extents.extend(ext)
        kind = classify(rel) if home_like else "file: " + (os.path.splitext(rel)[1] or "(none)")
        if kind == "store pack":
            records, named, pad = pack_records(path, rel)
            values.extend(records)
            externals.update(named)
            padding += pad
            continue
        value = Value(kind, rel, st.st_size, own, file_digest(path), path, 0, st.st_size)
        if linked:
            value.duplicate_of = f"hard link of {linked}"
        elif st.st_size and own == 0:
            value.duplicate_of = "a clone: every block is an earlier file's"
        file_values[rel] = value
        values.append(value)

    # A whole file of the store is of the kind of the record that names it.
    for value in values:
        name = os.path.basename(value.where)
        if value.kind == "store whole file" and name.startswith("file-"):
            kind = externals.get(name[5:])
            if kind is not None:
                value.kind = f"store whole file: {KINDS[kind]}"

    # Equal content under two names: by the store's content id among records, by a hash of the
    # bytes among files and raw records.
    first_record: dict[bytes, str] = {}
    first_bytes: dict[bytes, str] = {}
    for value in values:
        if value.kind.startswith("store record"):
            if value.kind.endswith("(external)"):
                continue
            if value.digest in first_record and not value.duplicate_of:
                value.duplicate_of = f"content of {first_record[value.digest]}"
            first_record.setdefault(value.digest, value.where)
            if value.kind.endswith("(raw)") and value.length >= 4096:
                first_bytes.setdefault(digest(read_value(value)), value.where)
    for value in values:
        if value.kind.startswith("store record") or value.length == 0:
            continue
        if value.digest in first_bytes and not value.duplicate_of:
            value.duplicate_of = f"bytes of {first_bytes[value.digest]}"
        first_bytes.setdefault(value.digest, value.where)

    kinds: dict[str, dict] = {}
    for value in values:
        row = kinds.setdefault(
            value.kind, {"values": 0, "stored": 0, "own": 0, "duplicates": 0, "duplicate_bytes": 0}
        )
        row["values"] += 1
        row["stored"] += value.stored
        row["own"] += value.own
        if value.duplicate_of:
            row["duplicates"] += 1
            row["duplicate_bytes"] += value.stored
    if padding:
        kinds["store pack: headers and commit padding"] = {
            "values": 0, "stored": padding, "own": padding, "duplicates": 0, "duplicate_bytes": 0,
        }  # fmt: skip
    for kind, row in kinds.items():
        row.update(compressed([v for v in values if v.kind == kind and v.length], zstd))

    dirs = {
        d: {**row, "on_disk": union(dir_extents[d])} for d, row in sorted(by_dir.items())
    }
    return {
        "total": {
            "files": len(files),
            "length": sum(st.st_size for _, _, st in files),
            "blocks": sum(row["blocks"] for row in by_dir.values()),
            "on_disk": union(all_extents),
        },
        "directories": dirs,
        "kinds": dict(sorted(kinds.items(), key=lambda it: -it[1]["stored"])),
        "largest": [
            {"where": v.where, "kind": v.kind, "stored": v.stored, "duplicate_of": v.duplicate_of}
            for v in sorted(values, key=lambda v: -v.stored)[:25]
        ],
        "duplicates": [
            {"where": v.where, "kind": v.kind, "stored": v.stored, "duplicate_of": v.duplicate_of}
            for v in sorted((v for v in values if v.duplicate_of), key=lambda v: -v.stored)[:25]
        ],
    }


def merged(spans: list[tuple[int, int]]) -> list[tuple[int, int]]:
    out: list[tuple[int, int]] = []
    for start, length in sorted(spans):
        if out and start <= out[-1][0] + out[-1][1]:
            last = out[-1]
            out[-1] = (last[0], max(last[1], start + length - last[0]))
        else:
            out.append((start, length))
    return out


def zstd_run(zstd: str, args: list[str], data: bytes | None = None) -> bytes:
    done = subprocess.run([zstd, "-q", *args], input=data, capture_output=True)
    if done.returncode:
        fail(SCRIPT, f"zstd {' '.join(args)} failed: {done.stderr.decode(errors='replace')[:400]}")
    return done.stdout


def compressed(values: list[Value], zstd: str) -> dict:
    """What a kind's values come to through zstd: as one stream, each alone, each with a dictionary."""
    if not values:
        return {}
    out: dict = {"raw": sum(v.length for v in values)}
    picked = values if len(values) <= SAMPLES_MOST else values[:: len(values) // SAMPLES_MOST + 1]
    out["sampled"] = len(picked)
    with tempfile.TemporaryDirectory(prefix="diskbytes-") as scratch:
        samples = os.path.join(scratch, "in")
        os.mkdir(samples)
        stream = os.path.join(scratch, "stream")
        with open(stream, "wb") as whole:
            for i, value in enumerate(picked):
                data = read_value(value)
                whole.write(data)
                with open(os.path.join(samples, f"{i:06}"), "wb") as f:
                    f.write(data)
        out["sampled_raw"] = os.path.getsize(stream)
        dictionary = os.path.join(scratch, "dictionary")
        trained = False
        if len(picked) >= DICTIONARY_FROM:
            done = subprocess.run(
                [zstd, "-q", "--train", "-r", samples, "-o", dictionary, "--maxdict=112640"],
                capture_output=True,
            )
            trained = done.returncode == 0 and os.path.exists(dictionary)
            if not trained:
                out["dictionary_error"] = done.stderr.decode(errors="replace").strip()[-200:]
        for level in LEVELS:
            long = ["--long=27"] if out["sampled_raw"] > (8 << 20) else []
            out[f"stream_{level}"] = len(zstd_run(zstd, [f"-{level}", *long, "-c", stream]))
            out[f"each_{level}"] = each(zstd, samples, scratch, [f"-{level}"])
            if trained:
                out[f"dictionary_{level}"] = each(zstd, samples, scratch, [f"-{level}", "-D", dictionary])
        if trained:
            out["dictionary_bytes"] = os.path.getsize(dictionary)
    return out


def each(zstd: str, samples: str, scratch: str, args: list[str]) -> int:
    """The sum of every sample compressed alone with `args`."""
    dest = os.path.join(scratch, "out")
    shutil.rmtree(dest, ignore_errors=True)
    os.mkdir(dest)
    zstd_run(zstd, [*args, "-r", samples, "--output-dir-flat", dest])
    return sum(os.path.getsize(os.path.join(dest, name)) for name in os.listdir(dest))


# ---------------------------------------------------------------------------------------------
# The session.

CARGO_TOML = '[package]\nname = "hello"\nversion = "0.0.0"\nedition = "2024"\n'
MAIN = 'fn main() {\n    // note 0\n    println!("hello: {}", (1..=10).sum::<u32>() + 100);\n}\n'


def session(it: Run) -> dict:
    work = os.path.realpath(it.work)
    if work != os.path.abspath(it.work):
        fail(SCRIPT, f"work {it.work} is under a symlink ({work}): the daemon's no-op lane is off there")
    shutil.rmtree(work, ignore_errors=True)
    home, xdg, project = (os.path.join(work, name) for name in ("home", "xdg", "hello"))
    os.makedirs(os.path.join(project, "src"))
    os.makedirs(home)
    os.makedirs(xdg)
    with open(os.path.join(project, "Cargo.toml"), "w") as f:
        f.write(CARGO_TOML)
    main = os.path.join(project, "src", "main.rs")
    with open(main, "w") as f:
        f.write(MAIN)
    env = {
        "HOME": os.environ["HOME"],
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        "BLAZE_HOME": home,
        "XDG_CACHE_HOME": xdg,
        "TMPDIR": os.environ.get("TMPDIR", "/tmp"),
    }
    blaze = os.path.join(it.bin, "blaze")
    rows: list[dict] = []
    bad = 0

    def turn(what: str, expect: int) -> None:
        nonlocal bad
        start = time.monotonic()
        done = subprocess.run([blaze, "run", "-q"], cwd=project, env=env, capture_output=True, text=True)
        seconds = time.monotonic() - start
        ok = done.returncode == 0 and done.stdout.strip().endswith(f"hello: {expect}")
        if not ok:
            bad += 1
            print(f"{SCRIPT}: {what}: exit {done.returncode}\n{done.stdout[-600:]}\n{done.stderr[-1200:]}")
        rows.append({"what": what, "seconds": seconds, "ok": ok})
        time.sleep(it.pause_seconds)

    def edit(pattern: str, replacement: str) -> None:
        with open(main) as f:
            source = f.read()
        changed, n = re.subn(pattern, replacement, source, count=1)
        if n != 1 or changed == source:
            fail(SCRIPT, f"the edit {pattern!r} changed nothing")
        # In place, as an editor's save of an open file is.
        with open(main, "r+") as f:
            f.write(changed)
            f.truncate()

    turn("cold build", 155)
    turn("no-op", 155)
    snapshots = {"after the build": quick(home, xdg)}
    for i in range(1, it.body_edits + 1):
        edit(r"\+ \d+\)", f"+ {400 + i})")
        # The first edit after a cold build does work no later one does: a row of its own.
        turn("first body edit" if i == 1 else "body edit", 455 + i)
    last = 455 + it.body_edits if it.body_edits else 155
    snapshots["after the body edits"] = quick(home, xdg)
    for i in range(1, it.comment_edits + 1):
        edit(r"// note \d+", f"// note {i}")
        turn("comment edit", last)
    time.sleep(it.settle_seconds)
    snapshots["after the comment edits, settled"] = quick(home, xdg)
    if it.stop:
        pids = daemons(home)
        start = time.monotonic()
        done = subprocess.run([blaze, "stop"], cwd=project, env=env, capture_output=True, text=True)
        rows.append({"what": "stop", "seconds": time.monotonic() - start, "ok": done.returncode == 0})
        if done.returncode:
            bad += 1
            print(f"{SCRIPT}: stop: exit {done.returncode}\n{done.stdout[-600:]}\n{done.stderr[-600:]}")
        # `stop` answers before the daemon's process has gone (agents.json rev-3): the bytes
        # are read once it has, so nothing is still being written or removed.
        gone = time.monotonic() + 120
        while any(alive(pid) for pid in pids):
            if time.monotonic() > gone:
                fail(SCRIPT, f"the daemon (pid {pids}) is still there 120 s after `blaze stop` answered")
            time.sleep(0.1)
        rows.append({"what": "stop to exit", "seconds": time.monotonic() - start, "ok": True})
        snapshots["after stop"] = quick(home, xdg)
    medians = {
        what: {
            "n": len(times),
            "median_ms": round(statistics.median(times) * 1000, 1),
            "min_ms": round(min(times) * 1000, 1),
            "max_ms": round(max(times) * 1000, 1),
        }
        for what in dict.fromkeys(row["what"] for row in rows)
        if (times := [row["seconds"] for row in rows if row["what"] == what])
    }
    tables = measure([(home, ""), (os.path.join(xdg, "blaze"), "machine")], True, it.zstd)
    return {
        "commit": it.commit,
        "machine": it.machine or f"{platform.system()} {platform.machine()} {os.cpu_count()} cores",
        "bin": it.bin,
        "home": home,
        "bad_runs": bad,
        "edit_loop": medians,
        "turns": rows,
        "snapshots": snapshots,
        **tables,
    }


def daemons(home: str) -> list[int]:
    """The pids of the daemons of `home`: each has a flight recorder's mapping named for it."""
    profile = os.path.join(home, "v2", "profile")
    found = [re.fullmatch(r"flight-(\d+)\.map", name) for name in os.listdir(profile)]
    pids = [int(it.group(1)) for it in found if it]
    if not pids:
        fail(SCRIPT, f"no daemon's flight recorder is in {profile}: which process to wait for is not known")
    return pids


def alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


def quick(home: str, xdg: str) -> dict[str, int]:
    """Bytes of blocks by top directory, each inode once (as `du` says them: clones counted twice)."""
    out: dict[str, int] = {}
    seen: set[tuple[int, int]] = set()
    for root, top in ((home, ""), (os.path.join(xdg, "blaze"), "machine")):
        for path, rel, st in walk(root, top, running=True):
            if (st.st_dev, st.st_ino) in seen:
                continue
            seen.add((st.st_dev, st.st_ino))
            parts = rel.split("/")
            key = "/".join(parts[:2]) if parts[0] == "v2" and len(parts) > 2 else parts[0]
            out[key] = out.get(key, 0) + st.st_blocks * 512
    out["total"] = sum(out.values())
    return out


# ---------------------------------------------------------------------------------------------
# The tables, printed.


def mb(n: int | None) -> str:
    return "-" if n is None else f"{n / 1e6:9.2f}"


def show(result: dict, title: str) -> None:
    print(f"\n== {title}")
    total = result["total"]
    print(
        f"total: {total['files']} files, {mb(total['length'])} MB long, {mb(total['blocks'])} MB of blocks "
        f"as du counts them, {mb(total['on_disk'])} MB on disk (shared blocks once)"
    )
    if "edit_loop" in result:
        print("\nedit loop (wall time of `blaze run -q`):")
        for what, row in result["edit_loop"].items():
            print(f"  {what:14} n={row['n']:2} median {row['median_ms']:9.1f} ms  ({row['min_ms']:.1f} to {row['max_ms']:.1f})")
        print(f"  bad runs: {result['bad_runs']}")
        print("\nblocks by directory as the session went (MB, du's count):")
        names = list(dict.fromkeys(k for snap in result["snapshots"].values() for k in snap))
        print("  " + " ".join(f"{name[:14]:>14}" for name in names))
        for when, snap in result["snapshots"].items():
            print("  " + " ".join(f"{snap.get(name, 0) / 1e6:14.1f}" for name in names) + f"  {when}")
    print("\nby directory (MB):")
    print(f"  {'directory':44} {'files':>6} {'length':>9} {'blocks':>9} {'on disk':>9}")
    for name, row in result["directories"].items():
        print(f"  {name[:44]:44} {row['files']:6} {mb(row['length'])} {mb(row['blocks'])} {mb(row['on_disk'])}")
    print("\nby kind (MB; zstd of the kind's values: one stream / each alone / each with a trained dictionary):")
    head = f"  {'kind':42} {'values':>6} {'stored':>9} {'own':>9} {'dup':>9}"
    for level in LEVELS:
        head += f" {'s' + str(level):>8} {'e' + str(level):>8} {'d' + str(level):>8}"
    print(head)
    for name, row in result["kinds"].items():
        line = f"  {name[:42]:42} {row['values']:6} {mb(row['stored'])} {mb(row['own'])} {mb(row['duplicate_bytes'])}"
        for level in LEVELS:
            for key in ("stream", "each", "dictionary"):
                n = row.get(f"{key}_{level}")
                # A sampled kind's figures are scaled to the whole kind.
                if n is not None and row.get("sampled_raw") and row["sampled_raw"] != row["raw"]:
                    n = int(n * row["raw"] / row["sampled_raw"])
                line += f" {mb(n)[1:]:>8}"
        print(line)
    print("\nlargest duplicates:")
    for row in result["duplicates"][:12]:
        print(f"  {mb(row['stored'])} MB  {row['where'][:70]}  [{row['kind']}]  {row['duplicate_of'][:80]}")


def main() -> None:
    match sys.argv[1:]:
        case ["run", text]:
            it = parse(Run, text, SCRIPT)
            result = session(it)
            title = f"blaze {it.commit} on {result['machine']}"
        case ["home", text]:
            at = parse(Home, text, SCRIPT)
            if not os.path.isdir(at.home):
                fail(SCRIPT, f"{at.home} is no directory")
            result = measure([(at.home, ""), (at.machine_dir, "machine")], True, at.zstd)
            result["home"] = at.home
            title = at.label or at.home
            it = at
        case ["tree", text]:
            tree = parse(Tree, text, SCRIPT)
            if not os.path.isdir(tree.dir):
                fail(SCRIPT, f"{tree.dir} is no directory")
            result = measure([(tree.dir, "")], False, tree.zstd)
            result["dir"] = tree.dir
            title = tree.label or tree.dir
            it = tree
        case _:
            fail(SCRIPT, "usage: diskbytes.py run|home|tree '<json>' | FILE | -")
    os.makedirs(os.path.dirname(os.path.abspath(it.out)), exist_ok=True)
    with open(it.out, "w") as f:
        json.dump(result, f, indent=1)
    show(result, title)
    if result.get("bad_runs"):
        fail(SCRIPT, f"{result['bad_runs']} runs gave a wrong answer or failed: the numbers are of a broken session")


if __name__ == "__main__":
    main()
