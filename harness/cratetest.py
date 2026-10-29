#!/usr/bin/env python3
"""Differential test using a crate's own integration tests.

usage: cratetest.py <crate-dir> [--crate <name>] [--features a,b] [--skip <test>]... [--bisect]
                    [--json out.json]

Each `tests/*.rs` target is built by cargo, then relinked with blaze's objects for the crate in
front of rustc's rlib. Both binaries must report the same result for every test.
A crate whose tests print something else in every run of rustc's own binary is UNSTABLE, not a
mismatch.
"""
import argparse, json, os, re, subprocess, sys
from difftest import Fixture, bucket, compare, discard, RUNNER


UNIT = "(unit tests)"


class TestFixture(Fixture):
    def __init__(self, crate_dir, crate, test, path, shared, features=None, skip=()):
        if test == UNIT:
            # The crate itself, built with `cfg(test)`: its `#[cfg(test)]` modules are the tests.
            super().__init__(crate_dir, [crate], ["--lib", "--profile", "test"],
                             touch=path)
            self.blaze_args = ["--unit-test"]
        else:
            # By its file: an example may have the name of the test.
            super().__init__(crate_dir, [crate, os.path.abspath(path)], ["--test", test], touch=path)
            self.blaze_args = ["--dev-deps"]
        if features:
            self.target_args += ["--features", features]
            self.blaze_args += ["--features", features]
        # A test left out by name: `--exact` keeps a name from leaving out the ones it starts.
        self.skips = ["--exact", *(x for name in skip for x in ("--skip", name))] if skip else []
        self.run_args = ["--test-threads=1", *self.skips]
        self.shared = shared

    def rustc_build(self):
        super().rustc_build()
        # A test that names nothing of the crate is linked without it: there is nothing of the
        # crate in the binary for blaze to stand in front of.
        # The standard library's own copy of a crate of the same name (hashbrown) is not it.
        linked = any(os.path.basename(r).startswith(f"lib{self.crates[0]}-") and "/rustlib/" not in r
                     for r in self.rlibs)
        if len(self.crates) == 2 and not linked:
            print(f"{self.crates[0]} is not linked into this test")
            self.crates = self.crates[1:]

    def binary_path(self):
        # Beside cargo's: a test may look for the crate's other artifacts from where it runs
        # (autocfg probes the rlibs in the directory of `current_exe`).
        return self.reference + "-blaze"

    def blaze_build(self, deny=()):
        # The crate's objects are the same whichever test binary they are linked into.
        # blaze links to what the rlibs already hold, so that only goes for the same rlibs.
        key = tuple(self.rlibs)
        if deny or key not in self.shared:
            super().blaze_build(deny)
            if self.leaves_to_rustc():
                # rustc lays a future out its own way, so what the test leaves to rustc decides
                # what of the crate can be blaze's: one run has to see both.
                self.crates = [",".join(self.crates)]
                super().blaze_build(deny)
            if not deny:
                self.shared[key] = (self.objects, self.reports)
        else:
            objects, self.reports = self.shared[key]
            self.link(objects)

    def leaves_to_rustc(self):
        """Whether the test, compiled apart from the crate, left a function of its own to rustc."""
        rows = self.reports[self.crates[-1]] if len(self.crates) == 2 else []
        return any(r[0] == "skipped" and bucket(r[2]) != "no rustc symbol for function" for r in rows)

    def normalize(self, stdout):
        # A test that prints the path it runs from prints the name blaze's binary has.
        stdout = stdout.replace(os.fsencode(self.binary_path()), os.fsencode(self.reference))
        stdout = re.sub(rb"' \(\d+\) panicked", b"' panicked", stdout)
        return re.sub(rb"finished in [0-9.]+s", b"finished", stdout)


def differing_tests(fx):
    """Runs every test on its own in both binaries: one that aborts hides the ones after it."""
    listed = subprocess.run([*RUNNER, fx.reference, "--list"], capture_output=True, text=True).stdout
    names = [l[:-len(": test")] for l in listed.splitlines() if l.endswith(": test")]
    differing = []
    for name in names:
        if name in fx.skips:
            continue
        fx.run_args = ["--test-threads=1", "--exact", name]
        want, got = fx.behaviour(fx.reference), fx.behaviour(fx.binary)
        if want[:2] != got[:2]:
            tail = got[2].decode(errors="replace").strip().splitlines()[-3:]
            differing.append(name)
            print(f"  DIFFERS {name}: rustc status {want[0]}, blaze status {got[0]}: {' | '.join(tail)[:300]}")
    fx.run_args = ["--test-threads=1", *fx.skips]
    return differing


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("crate_dir")
    ap.add_argument("--crate")
    ap.add_argument("--bisect", action="store_true")
    ap.add_argument("--test", action="append", help="only these test binaries; `unit` is the crate's own")
    ap.add_argument("--features", help="cargo features to turn on, comma separated")
    ap.add_argument("--skip", action="append", default=[], help="a test, by its whole name, to leave out")
    ap.add_argument("--json")
    a = ap.parse_args()
    crate = a.crate or os.path.basename(os.path.abspath(a.crate_dir)).replace("-", "_")
    meta = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"], cwd=a.crate_dir))
    package = next(p for p in meta["packages"] if p["name"].replace("-", "_") == crate
                   or any(t["name"].replace("-", "_") == crate for t in p["targets"]))
    # The test targets as cargo sees them: one may be named otherwise than its file, or be a
    # directory.
    tests = sorted((t["name"], t["src_path"]) for t in package["targets"] if "test" in t["kind"])
    lib_target = next((t for t in package["targets"] if "lib" in t["kind"]), None)
    lib = lib_target and lib_target["src_path"]
    if lib_target and not a.crate:
        # A package may name its library otherwise (`md-5` has `md5`, `redox_syscall` `syscall`).
        crate = lib_target["name"].replace("-", "_")
    if lib:
        tests.append((UNIT, lib))
    tests = [(t, path) for t, path in tests if not a.test or t in a.test or (t == UNIT and "unit" in a.test)]
    results, shared, left_out = [], {}, []
    for test, path in tests:
        print(f"--- {test}", flush=True)
        fx = TestFixture(a.crate_dir, crate, test, path, shared, a.features, a.skip)
        try:
            result = compare(fx, test, a.bisect)
        except SystemExit as e:
            # One test target that does not build or link must not hide the others.
            # blaze sees the crate with its default features, so only those are tested.
            if "requires the features" in str(e):
                print("left out: needs features that are off by default")
                left_out.append({"test": test, "why": "needs features"})
                continue
            # rustc could not build it either: a dev-dependency the published crate lacks, say.
            if str(e).startswith("cargo failed"):
                print(f"left out: rustc does not build it: {str(e)[-300:]}")
                errors = re.findall(r"^error(?:\[E\d+\])?: .*", str(e), re.M)
                left_out.append({"test": test, "why": "cargo fails", "error": (errors or [str(e)[-200:]])[0][:200]})
                continue
            print(f"ERROR: {str(e)[:1500]}")
            results.append({"match": False, "tests_run": 0, "error": str(e)[:300], "crates": {},
                            "work": fx.work})
            continue
        if not result["match"]:
            # Tests that print a port, a random number or what a timeout made of the run
            # differ between two runs of rustc's binary too, which says nothing of blaze.
            runs = [fx.behaviour(fx.reference) for _ in range(3)]
            if any(run != runs[0] for run in runs):
                result["unstable"] = True
                print("  unstable: rustc's binary does not print the same twice")
            result["differing_tests"] = differing_tests(fx)
        expected = fx.behaviour(fx.reference)[1]
        result["tests_run"] = len(re.findall(rb"^test .* \.\.\. ", expected, re.M))
        results.append(result)
    ok = all(r["match"] for r in results)
    total = sum(r["tests_run"] for r in results)
    merged = {"fixture": crate, "match": ok, "tests_run": total,
              "crates": results[0]["crates"] if results else {},
              "binaries": results, "left_out": left_out,
              "culprits": [c for r in results for c in r.get("culprits", [])]}
    # A crate blaze compiled nothing of matches rustc by construction, which is no result.
    untested = ok and not sum(c["compiled"] for r in results for c in r["crates"].values())
    unstable = not ok and all(r["match"] or r.get("unstable") for r in results)
    if unstable:
        merged["unstable"] = True
        print(f"UNSTABLE: {len(results)} test binaries, {total} tests, rustc's differ from run to run")
    elif untested:
        merged["untested"] = True
        print(f"UNTESTED: {len(results)} test binaries, {total} tests, no function from blaze")
    else:
        print(f"{'MATCH' if ok else 'MISMATCH'}: {len(results)} test binaries, {total} tests")
    if a.json:
        json.dump(merged, open(a.json, "w"), indent=1)
    discard(results)
    sys.exit(3 if unstable else 2 if untested else 0 if ok else 1)


if __name__ == "__main__":
    main()
