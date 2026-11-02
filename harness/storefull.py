#!/usr/bin/env python3
"""storefull.py: v2's store on a disk that has no room. Linux: the disk is a tmpfs mounted with
`sudo -n`.

    uv run --frozen python harness/storefull.py '{"blaze": "target/release/blaze"}'

`blaze` is the v2 command line. The input is one JSON object, inline, a file's path, or `-`.

The daemon's store directory, `<home>/v2/store`, is a small file system of its own, so only the
store runs out of room; the VFS's journal and the database's image are on the disk around it. A
file in the store directory, `.ballast` (named by no head, so read by no store), takes up all but
so many bytes, and the daemon runs out at another place each time: when it writes its first
head, in a pack, in an index, at a commit. blaze-cas retries a commit that failed and breaks
after FAILURES_MOST failures in a row (crates/blaze-cas/src/lib.rs, `Broken`), then serves what it
has mapped and makes nothing durable. For each room:

  - the daemon refuses to start, saying the store's file it could not write and that the disk
    is full; or it starts, and each of its builds (the first and one after an edit) either
    prints what blaze's build from nothing prints or stops saying the same;
  - nothing panics, and after `blaze stop` no temporary file of a whole-file write is left;
  - with room made, the next daemon on the same store opens it and builds right: a full disk
    never leaves a store that does not open.

Then room is made under a running daemon whose store has broken: its builds stay right, and the
next daemon's store keeps what it compiles (its packs hold at least half of what a cold build
writes), which the daemon after it builds from.
"""
import errno, os, shutil, subprocess, sys, time
from dataclasses import dataclass

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from typed_input import fail, parse
from storelib import (Checkout, Daemon, Failed, blaze_of, files, forget, named, references, scratch,
                      stored_bytes, store_of)

SCRIPT = "storefull.py"
SIZE = 64 << 20
# The room left, in bytes; and two thirds of what a cold build writes, measured.
ROOMS = [0, 4096, 16 << 10, 128 << 10, 512 << 10]
NO_ROOM = "No space left on device"
BALLAST = ".ballast"
VALUES = [0, 1, 2, 3, 4, 5, 6, 7]


@dataclass(frozen=True)
class Input:
    blaze: str


class Findings(Exception):
    """What was wrong, said whole."""


def bad(what):
    raise Findings(f"FAIL: {what}")


class Disk:
    """A tmpfs of SIZE bytes mounted at the store's directory, the user's alone."""

    def __init__(self, at):
        self.at = at
        os.makedirs(at, exist_ok=True)
        options = f"size={SIZE},uid={os.getuid()},gid={os.getgid()},mode=700"
        mounted = subprocess.run(["sudo", "-n", "mount", "-t", "tmpfs", "-o", options, "tmpfs", at],
                                 capture_output=True, text=True)
        if mounted.returncode != 0:
            raise Failed(f"a small file system could not be mounted at {at} (passwordless sudo is needed): "
                         f"{mounted.stderr.strip()}")
        self.ballast = os.path.join(at, BALLAST)

    def empty(self):
        for name in os.listdir(self.at):
            path = os.path.join(self.at, name)
            shutil.rmtree(path) if os.path.isdir(path) and not os.path.islink(path) else os.remove(path)

    def free(self):
        return shutil.disk_usage(self.at).free

    def leave(self, room):
        """Takes up all of the disk but `room` bytes, to the block; the bytes left."""
        if os.path.exists(self.ballast):
            os.remove(self.ballast)
        take = self.free() - room
        if take < 0:
            raise Failed(f"the disk has {self.free()} bytes free, fewer than {room}")
        chunk = 1 << 20
        fd = os.open(self.ballast, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "wb", buffering=0) as ballast:
            while take:
                size = min(chunk, take)
                try:
                    ballast.write(bytes(size))
                except OSError as full:
                    if full.errno != errno.ENOSPC:
                        raise
                    if chunk == 4096:
                        break
                    chunk = 4096
                    continue
                take -= size
        return self.free()

    def room(self):
        if os.path.exists(self.ballast):
            os.remove(self.ballast)

    def unmount(self):
        subprocess.run(["sudo", "-n", "umount", self.at], capture_output=True)


class Run:
    def __init__(self, blaze, work):
        self.blaze, self.work = blaze, work
        self.home = os.path.join(work, "h")
        self.checkout = Checkout(work)
        self.expected = references(blaze, work, VALUES)
        self.disk = Disk(store_of(self.home))

    def daemon(self, name):
        return Daemon(self.blaze, self.home, name=name, wait=False)

    def right(self, daemon, built, what):
        got, want = built.program(), self.expected[self.checkout.value]
        if built.status != 0 or got != want:
            bad(f"{what}: the build ended {built.status} and its program prints\n{got}\nand blaze's build from "
                f"nothing\n{want}\nsaid: {built.said[-2000:]}\nlog: {daemon.log_path}")

    def full_or_right(self, daemon, what):
        """A build on a full store: right, or stopped saying the store is full; which."""
        built = daemon.build(self.checkout.ws)
        if built.status != 0:
            said = built.said + daemon.log()
            if NO_ROOM not in said or not named(said, self.disk.at):
                bad(f"{what}: the build stopped ({built.status}) and does not say the store's disk is full:\n"
                    f"{built.said[-2000:]}\nlog: {daemon.log_path}")
            return "stopped"
        self.right(daemon, built, what)
        return "right"

    def stopped(self, daemon, what):
        daemon.stop()
        if "panicked" in daemon.log():
            bad(f"{what}: the daemon panicked: {daemon.log_path}")
        if files(self.disk.at, "temporary"):
            bad(f"{what}: a part of a file was left: {files(self.disk.at, 'temporary')}")

    def cold(self):
        """How many bytes a cold build writes to a store with room."""
        self.disk.empty()
        forget(self.home)
        daemon = self.daemon("cold")
        if not daemon.wait():
            bad(f"with room, the daemon did not start: {daemon.log_path}")
        self.checkout.set(0)
        self.right(daemon, daemon.build(self.checkout.ws), "with room, to measure")
        self.stopped(daemon, "with room")
        return stored_bytes(self.disk.at)

    def without(self, room):
        """A daemon on a store with `room` bytes; then one with room on the same store."""
        self.disk.empty()
        forget(self.home)
        left = self.disk.leave(room)
        what = f"{room} bytes of room"
        daemon = self.daemon(f"room-{room}")
        if not daemon.wait():
            said = daemon.log()
            if NO_ROOM not in said or not named(said, self.disk.at):
                bad(f"{what}: the daemon refused to start without saying the store's disk is full:\n{said[-2000:]}")
            how = ["refused to start"]
        else:
            self.checkout.set(room % 4)
            how = [self.full_or_right(daemon, f"{what}, the first build")]
            self.checkout.set(room % 4 + 4)
            how.append(self.full_or_right(daemon, f"{what}, after an edit"))
        self.stopped(daemon, what)
        self.disk.room()
        again = self.daemon(f"room-{room}-again")
        if not again.wait():
            bad(f"{what}: with room made the store does not open:\n{again.log()[-2000:]}")
        self.right(again, again.build(self.checkout.ws), f"{what}, with room made")
        self.stopped(again, f"{what}, with room made")
        print(f"  {left:>8} bytes free: {', '.join(how)}; with room the store opened and built right", flush=True)

    def kept_again(self, cold):
        """Room made under a daemon whose store is full; the next keeps what it compiles."""
        self.disk.empty()
        forget(self.home)
        self.disk.leave(128 << 10)
        daemon = self.daemon("again")
        if not daemon.wait():
            bad(f"128 KiB of room: the daemon did not start, so no room can be made under it: {daemon.log_path}")
        self.checkout.set(1)
        self.full_or_right(daemon, "before room is made")
        self.disk.room()
        self.checkout.set(2)
        self.right(daemon, daemon.build(self.checkout.ws), "after room was made under the daemon")
        self.stopped(daemon, "room made under it")
        forget(self.home)
        daemon = self.daemon("kept")
        if not daemon.wait():
            bad(f"the store a full disk left does not open: {daemon.log_path}")
        self.checkout.set(3)
        self.right(daemon, daemon.build(self.checkout.ws), "the next daemon, with room")
        self.stopped(daemon, "the next daemon")
        kept = stored_bytes(self.disk.at)
        if kept < cold // 2:
            bad(f"with room again the daemon kept {kept} bytes of a cold build's {cold}: {daemon.log_path}")
        forget(self.home)
        daemon = self.daemon("from-kept")
        if not daemon.wait():
            bad(f"the store the daemon with room kept does not open: {daemon.log_path}")
        self.checkout.set(4)
        self.right(daemon, daemon.build(self.checkout.ws), "from the store kept with room")
        self.stopped(daemon, "from the store kept")
        print(f"  room made: {kept} bytes kept by the next daemon, and built from by the one after", flush=True)


def main(args):
    if sys.platform != "linux":
        fail(SCRIPT, f"Linux only (a tmpfs mounted with sudo), not {sys.platform}")
    blaze = blaze_of(args.blaze)
    start = time.time()
    work = scratch("blaze-storefull-")
    run = None
    try:
        run = Run(blaze, work)
        cold = run.cold()
        last = cold * 2 // 3
        rooms = [*(room for room in ROOMS if room < last), last]
        for room in rooms:
            run.without(room)
        run.kept_again(cold)
    except (Failed, Findings) as failed:
        sys.exit(f"{SCRIPT}: {failed}\n  kept: {work}")
    finally:
        if run is not None:
            run.disk.unmount()
    shutil.rmtree(work, ignore_errors=True)
    print(f"PASS: {len(rooms)} full stores, each refused or built right and opened again with room, "
          f"in {time.time() - start:.0f}s")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>'\n\n{__doc__}")
    main(parse(Input, sys.argv[1], SCRIPT))
