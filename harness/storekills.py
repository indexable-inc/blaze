#!/usr/bin/env python3
"""storekills.py: v2's daemon killed at random moments while it writes its store.

    uv run --frozen python harness/storekills.py '{"blaze": "target/release/blaze"}'
    uv run --frozen python harness/storekills.py '{"blaze": "...", "kills": 300, "workers": 4, "seed": 7}'

`blaze` is the v2 command line; `kills` (100) are shared among `workers` (4), each with a
checkout of the stress workspace and a home, so a store, of its own; `seed` (0: the clock)
picks the moments. The input is one JSON object, inline, a file's path, or `-`.

A kill is SIGKILL, at a random moment of one of:

  cold     the home emptied, a daemon started and asked for the program, killed before it
           answers
  start    a daemon started on the store as the kill before left it, killed while it opens
           it or just after it listens
  edit     a daemon that listens is asked for the program after an edit, and killed before,
           while or after it compiles and commits to its store
  settle   the same, with the kill around the moment it answers, after which it commits and,
           idle, settles its store
  write    a daemon asked for a program its store has not seen, killed at the moment the
           store's files are seen to change for the first, second or third time

After every kill a new daemon starts in the same home (half the time with the VFS's journal and
the database's image the kill left, half the time with the store alone): it has to open the
store, and its build has to print what blaze's build from nothing prints. A store it refuses
to open is a finding: a kill at any moment leaves a store whose head names only whole bytes
(docs/v2-store.md, "A group commit").
"""
import os, random, shutil, sys, threading, time
from dataclasses import dataclass

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from typed_input import fail, parse
from storelib import Checkout, Daemon, Failed, blaze_of, files, forget, listing, references, scratch, store_of

SCRIPT = "storekills.py"
MODES = ["cold", "start", "start", "edit", "edit", "settle", "write", "write"]
# The values an edit sets, which the store sees again and again, and those a `write` kill asks
# for once each until the store is emptied.
EDITS = range(6)
WRITES = range(100, 106)


@dataclass(frozen=True)
class Input:
    blaze: str
    kills: int = 100
    workers: int = 4
    seed: int = 0


class Worker:
    def __init__(self, blaze, seed, number, kills, expected):
        self.blaze, self.number, self.kills, self.expected, self.seed = blaze, number, kills, expected, seed
        self.rng = random.Random(seed * 1000 + number)
        self.work = scratch(f"blaze-storekills-{number}-")
        self.home = os.path.join(self.work, "h")
        self.checkout = Checkout(self.work)
        self.seen = set()
        self.counts = {mode: 0 for mode in set(MODES)}
        self.found = {"before it listened": 0, "after it listened": 0, "before it answered": 0, "after it answered": 0,
                      "as the store changed": 0, "with the store alone": 0, "temporary files left": 0,
                      "a build started a daemon of its own": 0}
        self.start_time, self.build_time = 2.0, 1.0
        self.failure = None

    def checked(self, what):
        """A new daemon in the home as the kill left it, once it has built the program right."""
        if self.rng.random() < 0.5:
            forget(self.home)
            self.found["with the store alone"] += 1
        self.found["temporary files left"] += bool(files(store_of(self.home), "temporary"))
        start = time.time()
        daemon = Daemon(self.blaze, self.home, wait=False)
        if not daemon.wait():
            raise Failed(f"FAIL {what}: the next daemon did not start on the store the kill left\n"
                         f"{daemon.log()[-2000:]}\nlog: {daemon.log_path}")
        self.start_time = 0.7 * self.start_time + 0.3 * (time.time() - start)
        self.build_right(daemon, what)
        return daemon

    def build_right(self, daemon, what):
        start = time.time()
        built = daemon.build(self.checkout.ws)
        got, want = built.program(), self.expected[self.checkout.value]
        if built.status != 0 or got != want:
            raise Failed(f"FAIL {what}: the build ended {built.status} and its program prints\n{got}\n"
                         f"and blaze's build from nothing\n{want}\nsaid: {built.said[-2000:]}\nlog: {daemon.log_path}")
        self.build_time = 0.7 * self.build_time + 0.3 * (time.time() - start)
        self.seen.add(self.checkout.value)

    def killed(self, daemon, client=None):
        """Kills the daemon; then, once its client has ended (a client that found nothing
        listening started a daemon of its own and waited for it), stops any such daemon."""
        daemon.kill()
        if client is not None:
            client.communicate()
        self.found["a build started a daemon of its own"] += daemon.stray()

    def kill_writing(self):
        """A daemon asked for a program the store has not seen, killed a moment after the
        store's files are seen to be other than they were."""
        unseen = [value for value in WRITES if value not in self.seen]
        if not unseen:
            shutil.rmtree(self.home, ignore_errors=True)
            self.seen.clear()
            unseen = list(WRITES)
        self.checkout.set(self.rng.choice(unseen))
        daemon = Daemon(self.blaze, self.home, wait=False)
        if not daemon.wait():
            raise Failed(f"FAIL worker {self.number}: a daemon did not start: {daemon.log_path}")
        store = store_of(self.home)
        before, give_up = listing(store), time.time() + 3 * self.build_time + 1
        client = daemon.asking(self.checkout.ws)
        changes = self.rng.randrange(1, 4)
        while time.time() < give_up and changes and client.poll() is None:
            now = listing(store)
            changes -= now != before
            before = now
        self.found["as the store changed" if not changes else "after it answered"] += 1
        time.sleep(self.rng.choice([0, 0, self.rng.uniform(0, 0.003)]))
        self.killed(daemon, client)

    def kill_starting(self, mode):
        if mode == "cold":
            shutil.rmtree(self.home, ignore_errors=True)
            self.seen.clear()
        daemon = Daemon(self.blaze, self.home, wait=False)
        if mode == "cold":
            # A cold daemon writes its store only once it builds: it is asked to, and killed
            # before it answers.
            if not daemon.wait():
                raise Failed(f"FAIL worker {self.number}: a daemon on an empty home did not start: {daemon.log_path}")
            client = daemon.asking(self.checkout.ws)
            time.sleep(self.rng.uniform(0, self.build_time))
            self.found["before it answered" if client.poll() is None else "after it answered"] += 1
            self.killed(daemon, client)
            return
        time.sleep(self.rng.uniform(0, self.start_time * 1.2))
        self.found["after it listened" if daemon.listens() else "before it listened"] += 1
        self.killed(daemon)

    def kill_building(self, daemon, mode):
        # Half of the edits are to a state some build before has seen, which is in the store.
        self.checkout.set(self.rng.choice(EDITS))
        client, start = daemon.asking(self.checkout.ws), time.time()
        low = 0.7 if mode == "settle" else 0.0
        kill_at = start + self.rng.uniform(low * self.build_time, 1.3 * self.build_time)
        while time.time() < kill_at and client.poll() is None:
            time.sleep(0.0005)
        answered = client.poll() is not None
        if answered:
            self.build_time = 0.7 * self.build_time + 0.3 * (time.time() - start)
            # What it commits and settles once it has answered.
            time.sleep(self.rng.uniform(0, 0.05 if mode == "settle" else 0.01))
        self.killed(daemon, client)
        self.found["after it answered" if answered else "before it answered"] += 1

    def run(self):
        daemon = None
        try:
            daemon = self.checked("before any kill")
            for kill in range(self.kills):
                mode = self.rng.choice(MODES)
                self.counts[mode] += 1
                if mode in ("cold", "start"):
                    self.killed(daemon)
                    self.kill_starting(mode)
                elif mode == "write":
                    self.killed(daemon)
                    self.kill_writing()
                else:
                    self.kill_building(daemon, mode)
                daemon = self.checked(f"worker {self.number}, kill {kill} ({mode}, seed {self.seed})")
            daemon.stop()
        except (Failed, Exception) as failure:
            self.failure = failure
            if daemon is not None:
                daemon.kill()


def main(args):
    blaze = blaze_of(args.blaze)
    if args.workers < 1 or args.kills < args.workers:
        fail(SCRIPT, f"kills ({args.kills}) has to be at least workers ({args.workers}), which is at least 1")
    seed = args.seed or int(time.time())
    start = time.time()
    work = scratch("blaze-storekills-")
    try:
        expected = references(blaze, work, [*EDITS, *WRITES])
    except Failed as failed:
        sys.exit(f"{SCRIPT}: {failed}; kept: {work}")
    workers = [Worker(blaze, seed, n, args.kills // args.workers + (n < args.kills % args.workers), expected)
               for n in range(args.workers)]
    threads = [threading.Thread(target=worker.run) for worker in workers]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    failures = [worker for worker in workers if worker.failure is not None]
    for worker in failures:
        print(f"{worker.failure}\n  kept: {worker.work}")
    for worker in workers:
        if worker.failure is None:
            shutil.rmtree(worker.work, ignore_errors=True)
    done = sum(sum(worker.counts.values()) for worker in workers)
    counts = {mode: sum(worker.counts[mode] for worker in workers) for mode in sorted(set(MODES))}
    found = {what: sum(worker.found[what] for worker in workers) for what in workers[0].found}
    print(f"kills: {done} {counts}")
    print(f"found: {found}")
    print(f"seed {seed}, {time.time() - start:.0f}s")
    if failures or done != args.kills:
        sys.exit(f"{SCRIPT}: FAIL")
    shutil.rmtree(work, ignore_errors=True)
    print(f"PASS: {done} kills; after each the next daemon opened the store and built what the build from nothing does")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>'\n\n{__doc__}")
    main(parse(Input, sys.argv[1], SCRIPT))
