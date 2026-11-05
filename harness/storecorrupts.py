#!/usr/bin/env python3
"""storecorrupts.py: the files of v2's store damaged on disk, and a head of another format.

    uv run --frozen python harness/storecorrupts.py '{"blaze": "target/release/blaze"}'
    uv run --frozen python harness/storecorrupts.py '{"blaze": "...", "rounds": 40, "seed": 7}'

`blaze` is the v2 command line, `rounds` (16) how many damages, `seed` (0: the clock) which.
The input is one JSON object, inline, a file's path, or `-`.

A daemon builds four states of the stress workspace into its store and stops. Then, round after
round, with no daemon running, one file of the store (`head`, `index-N`, `pack-NNNNNN` or
`file-<content>`, crates/blaze-cas/src/disk.rs) is damaged: a bit flipped, the file cut short,
bytes added to it, part of it zeroed, the whole of it replaced, or the file removed. Half the
time the daemon's VFS journal and database image are removed too, so what it has comes from the
store alone. Then a daemon starts in that home and is asked for every state, and each time one of:

  - it refuses to start, and what it says names the damaged file;
  - a build stops, and what it or the daemon says names the damaged file;
  - the build prints what blaze's build from nothing prints.

Anything else is a finding: a build that prints something else, or an error that names no
damaged file (no-fallback: a store never quietly serves what is not what it wrote, nor stops
without saying where; blaze_cas's `Corrupt` and `Broken`).

Then the head is made to say another format, older and newer: the daemon has to refuse to start,
naming the head and that format, and leave every file of the store as it was; with the head
put back it has to start and build right.
"""
import os, random, shutil, sys, time
from dataclasses import dataclass

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from typed_input import fail, parse
from storelib import Checkout, Daemon, Failed, blaze_of, files, forget, listing, named, references, scratch, store_of

SCRIPT = "storecorrupts.py"
STATES = [0, 1, 2, 3]
# Where the head says its format (crates/blaze-cas/src/disk.rs, `Head::encode`).
FORMAT_AT = 8


@dataclass(frozen=True)
class Input:
    blaze: str
    rounds: int = 16
    seed: int = 0


class Findings(Exception):
    """What was wrong, said whole."""


def flip(rng, data):
    at = rng.randrange(len(data))
    data[at] ^= 1 << rng.randrange(8)


def cut(rng, data):
    del data[rng.randrange(len(data)):]


def grow(rng, data):
    data += rng.randbytes(rng.randrange(1, 100))


def zero(rng, data):
    at = rng.randrange(len(data))
    end = min(len(data), at + rng.randrange(1, 4096))
    if not any(data[at:end]):
        data[at] = 1
    else:
        data[at:end] = bytes(end - at)


def replace(rng, data):
    data[:] = rng.randbytes(rng.randrange(0, 200))


DAMAGES = [flip, flip, cut, cut, grow, zero, replace]


def damage(rng, path):
    """Damages the file at `path` one way or another; how."""
    if rng.random() < 0.08:
        os.remove(path)
        return "removed"
    with open(path, "rb") as file:
        data = bytearray(file.read())
    how = rng.choice(DAMAGES) if data else grow
    how(rng, data)
    with open(path, "wb") as file:
        file.write(data)
    return how.__name__


class Run:
    def __init__(self, blaze, seed, work):
        self.blaze, self.seed, self.rng = blaze, seed, random.Random(seed)
        self.work = work
        self.home = os.path.join(work, "h")
        self.checkout = Checkout(work)
        self.expected = references(blaze, work, STATES)
        self.counts = {"refused, naming the file": 0, "a build stopped, naming the file": 0, "built right": 0}

    def fail(self, what):
        raise Findings(f"FAIL (seed {self.seed}): {what}")

    def right(self, daemon, built, what):
        got, want = built.program(), self.expected[self.checkout.value]
        if built.status != 0 or got != want:
            self.fail(f"{what}: the build ended {built.status} and its program prints\n{got}\nand blaze's build from "
                      f"nothing\n{want}\nsaid: {built.said[-2000:]}\nlog: {daemon.log_path}")

    def fill(self):
        """A store holding every state, written by a daemon that then stopped."""
        shutil.rmtree(self.home, ignore_errors=True)
        daemon = Daemon(self.blaze, self.home)
        if not daemon.listening:
            self.fail(f"a daemon on an empty home did not start: {daemon.log_path}")
        for state in STATES:
            self.checkout.set(state)
            self.right(daemon, daemon.build(self.checkout.ws), f"filling the store, state {state}")
        daemon.stop()
        if not files(store_of(self.home), "pack"):
            self.fail(f"a daemon built four states and its store has no pack: {os.listdir(store_of(self.home))}")

    def round(self, number):
        """Damages a file of the stopped daemon's store and builds every state; whether the store
        is still whole enough to go on with."""
        store = store_of(self.home)
        chosen = files(store, "pack") * 3 + files(store, "head", "index", "file")
        path = self.rng.choice(chosen)
        how = damage(self.rng, path)
        alone = self.rng.random() < 0.5
        if alone:
            forget(self.home)
        what = f"round {number}: {os.path.basename(path)} {how}{', the store alone' if alone else ''}"
        daemon = Daemon(self.blaze, self.home, name=f"round-{number}", wait=False)
        if not daemon.wait():
            said = daemon.log()
            if not named(said, path):
                self.fail(f"{what}: the daemon refused to start without naming {path}:\n{said[-2000:]}")
            self.counts["refused, naming the file"] += 1
            return False
        whole = True
        for state in self.rng.sample(STATES, len(STATES)):
            self.checkout.set(state)
            built = daemon.build(self.checkout.ws)
            if built.status != 0:
                said = built.said + daemon.log()
                if not named(said, path):
                    daemon.kill()
                    self.fail(f"{what}, state {state}: the build stopped ({built.status}) without naming {path}:\n"
                              f"{built.said[-2000:]}\nlog: {daemon.log_path}")
                self.counts["a build stopped, naming the file"] += 1
                whole = False
                break
            self.right(daemon, built, f"{what}, state {state}")
        else:
            self.counts["built right"] += 1
        daemon.stop()
        return whole

    def formats(self):
        """A head that says another format: refused, naming the head and the format, nothing
        changed; put back, built right."""
        self.fill()
        head = os.path.join(store_of(self.home), "head")
        original = open(head, "rb").read()
        current = int.from_bytes(original[FORMAT_AT:FORMAT_AT + 4], "little")
        for other, name in ((current - 1, "an older format"), (current + 1, "a newer format"), (0, "format 0")):
            changed = bytearray(original)
            changed[FORMAT_AT:FORMAT_AT + 4] = other.to_bytes(4, "little")
            open(head, "wb").write(changed)
            before = listing(store_of(self.home))
            daemon = Daemon(self.blaze, self.home, name=f"format-{other}", wait=False)
            if daemon.wait():
                daemon.kill()
                self.fail(f"{name}: a daemon started on a store whose head says format {other}: {daemon.log_path}")
            said = daemon.log()
            if not named(said, head) or f"format {other}" not in said:
                self.fail(f"{name}: the daemon's refusal does not name the head and format {other}:\n{said[-2000:]}")
            if listing(store_of(self.home)) != before:
                self.fail(f"{name}: the store's files changed under a daemon that refused it")
        open(head, "wb").write(original)
        daemon = Daemon(self.blaze, self.home, name="format-back")
        if not daemon.listening:
            self.fail(f"with its head put back the store does not open: {daemon.log_path}")
        self.checkout.set(self.rng.choice(STATES))
        self.right(daemon, daemon.build(self.checkout.ws), "with its head put back")
        daemon.stop()


def main(args):
    blaze = blaze_of(args.blaze)
    if args.rounds < 1:
        fail(SCRIPT, f"rounds: at least 1, not {args.rounds}")
    seed = args.seed or int(time.time())
    start = time.time()
    work = scratch("blaze-storecorrupts-")
    try:
        run = Run(blaze, seed, work)
        whole = False
        for number in range(args.rounds):
            if not whole or number % 6 == 0:
                run.fill()
            whole = run.round(number)
        run.formats()
    except (Failed, Findings) as failed:
        sys.exit(f"{SCRIPT}: {failed}\n  kept: {work}")
    shutil.rmtree(work, ignore_errors=True)
    print(f"damaged: {run.counts}; a head of an older, a newer and no format refused, the store left as it was")
    print(f"PASS: {args.rounds} damaged stores, each refused or built right (seed {seed}, {time.time() - start:.0f}s)")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>'\n\n{__doc__}")
    main(parse(Input, sys.argv[1], SCRIPT))
