#!/usr/bin/env python3
"""storeguards.py: v2's store is its user's alone. Linux and macOS.

    uv run --frozen python harness/storeguards.py '{"blaze": "target/release/blaze"}'

`blaze` is the v2 command line. The input is one JSON object, inline, a file's path, or `-`.

A daemon builds two states of the stress workspace into its store and stops. Then
(docs/v2/plan/lanes/store.md, "Done when": a store others can write to refused):

  - the store's directory is 0700 and every file in it (`head`, `index-N`, `pack-NNNNNN`,
    `file-<content>`) 0600;
  - a pack others can write to is not read: the next daemon refuses to start, or a build stops,
    naming the pack and saying others can write to it; or the pack is left unread, said so
    naming it, and the program built prints what blaze's build from nothing prints;
  - a store directory others can write to (0770, then 0707) is refused: the daemon does not
    start, says why naming the directory, and leaves every file of it as it was;
  - with the modes put back, a daemon opens the store and builds right.
"""
import os, re, shutil, stat, sys, time
from dataclasses import dataclass

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from typed_input import fail, parse
from storelib import Checkout, Daemon, Failed, blaze_of, files, listing, named, references, scratch, store_of

SCRIPT = "storeguards.py"
# How blaze says that others than the user can write to a file or directory.
OTHERS_WRITE = re.compile(r"can be written to by other users|belongs to another user", re.I)


@dataclass(frozen=True)
class Input:
    blaze: str


class Findings(Exception):
    """What was wrong, said whole."""


def bad(what):
    raise Findings(f"FAIL: {what}")


def mode(path):
    return stat.S_IMODE(os.stat(path).st_mode)


class Run:
    def __init__(self, blaze, work):
        self.blaze = blaze
        self.home = os.path.join(work, "h")
        self.store = store_of(self.home)
        self.checkout = Checkout(work)
        self.expected = references(blaze, work, [0, 1])

    def right(self, daemon, built, what):
        got, want = built.program(), self.expected[self.checkout.value]
        if built.status != 0 or got != want:
            bad(f"{what}: the build ended {built.status} and its program prints\n{got}\nand blaze's build from "
                f"nothing\n{want}\nsaid: {built.said[-2000:]}\nlog: {daemon.log_path}")

    def filled(self, name):
        daemon = Daemon(self.blaze, self.home, name=name)
        if not daemon.listening:
            bad(f"{name}: the daemon did not start: {daemon.log_path}")
        for value in (0, 1):
            self.checkout.set(value)
            self.right(daemon, daemon.build(self.checkout.ws), f"{name}, state {value}")
        daemon.stop()

    def the_users_alone(self):
        if mode(self.store) != 0o700:
            bad(f"the store {self.store} is {mode(self.store):o}, not 700")
        kept = files(self.store, "head", "index", "pack", "file")
        open_to = [(os.path.basename(path), f"{mode(path):o}") for path in kept if mode(path) != 0o600]
        if not kept or open_to:
            bad(f"files of the store are not 600: {open_to or 'there are none'}")
        print(f"  modes: the store 700, its {len(kept)} files 600", flush=True)

    def writable_pack(self):
        pack = files(self.store, "pack")[0]
        os.chmod(pack, 0o666)
        daemon = Daemon(self.blaze, self.home, name="writable-pack", wait=False)
        if not daemon.wait():
            said = daemon.log()
            if not named(said, pack) or not OTHERS_WRITE.search(said):
                bad(f"a pack others can write to: the daemon refused to start without saying so of {pack}:\n{said[-2000:]}")
            how = "the daemon refused to start"
        else:
            self.checkout.set(1)
            built = daemon.build(self.checkout.ws)
            said = built.said + daemon.log()
            if not named(said, pack) or not OTHERS_WRITE.search(said):
                daemon.kill()
                bad(f"a pack others can write to: neither the build nor the daemon says it is not read, naming {pack}:\n"
                    f"{built.said[-2000:]}\nlog: {daemon.log_path}")
            if built.status == 0:
                self.right(daemon, built, "a pack others can write to, left unread")
                how = "left unread, the program right"
            else:
                how = "the build stopped"
            daemon.stop()
        os.chmod(pack, 0o600)
        print(f"  a pack others can write to: {how}, naming it", flush=True)

    def writable_store(self):
        for open_to in (0o770, 0o707):
            os.chmod(self.store, open_to)
            before = listing(self.store)
            daemon = Daemon(self.blaze, self.home, name=f"writable-{open_to:o}", wait=False)
            if daemon.wait():
                daemon.kill()
                os.chmod(self.store, 0o700)
                bad(f"a daemon started on a store others can write to ({open_to:o}): {daemon.log_path}")
            said = daemon.log()
            if not named(said, self.store) or not OTHERS_WRITE.search(said):
                bad(f"a {open_to:o} store: the daemon's refusal does not say others can write to {self.store}:\n{said[-2000:]}")
            if listing(self.store) != before:
                bad(f"a {open_to:o} store: its files changed under a daemon that refused it")
        os.chmod(self.store, 0o700)
        print("  a store others can write to (770, 707): refused, naming it, nothing changed", flush=True)

    def back(self):
        daemon = Daemon(self.blaze, self.home, name="back")
        if not daemon.listening:
            bad(f"with its modes put back the store does not open: {daemon.log_path}")
        self.checkout.set(0)
        self.right(daemon, daemon.build(self.checkout.ws), "with the modes put back")
        daemon.stop()
        self.the_users_alone()


def main(args):
    if os.name != "posix":
        fail(SCRIPT, "the modes of files are checked on Unix only")
    blaze = blaze_of(args.blaze)
    start = time.time()
    work = scratch("blaze-storeguards-")
    try:
        run = Run(blaze, work)
        run.filled("fill")
        run.the_users_alone()
        run.writable_pack()
        run.writable_store()
        run.back()
    except (Failed, Findings) as failed:
        sys.exit(f"{SCRIPT}: {failed}\n  kept: {work}")
    shutil.rmtree(work, ignore_errors=True)
    print(f"PASS: the store is its user's alone, in {time.time() - start:.0f}s")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>'\n\n{__doc__}")
    main(parse(Input, sys.argv[1], SCRIPT))
