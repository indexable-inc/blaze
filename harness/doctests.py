#!/usr/bin/env python3
"""The doctests of real crates, run by `blaze test --doc` and judged by their own results:
every doctest that runs passes (or is ignored), but those KNOWN names, which fail. Each crate
is run twice, the second run reusing the programs of the first, which has to end the same
way and print the same. Each crate's line says how many examples blaze built and how many it
did not: those fail, saying why, and are counted as refused rather than judged.

    BLAZE_BIN=../target/release/blaze-engine python3 doctests.py [--few] [name-version ...]

Each crate is unpacked by `vendor.sh` and built in a directory that is removed afterwards.
A crate whose doctests do not pass is WRONG and fails the run. `--few` is the first three crates."""
import os
import re
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
BLAZE = os.path.abspath(os.environ.get("BLAZE_BIN", os.path.join(HERE, "..", "target", "release", "blaze-engine")))
CRATES = [
    "semver-1.0.28", "either-1.19.0", "smallvec-1.16.3",
    "serde_core-1.0.229", "regex-1.13.1", "itertools-0.15.0", "chrono-0.4.45", "anyhow-1.0.104",
    "bytes-1.12.1", "thiserror-2.0.21", "log-0.4.34", "bitflags-2.13.2", "indexmap-2.14.2",
    "once_cell-1.21.4", "hashbrown-0.17.1", "memchr-2.8.3", "url-2.5.8", "uuid-1.28.0",
    "time-0.3.55", "rand-0.10.3", "arrayvec-0.7.8", "base64-0.23.1",
    "byteorder-1.5.0", "num-traits-0.2.19", "strum-0.28.0", "nom-8.0.0", "textwrap-0.16.4",
    "unicode-segmentation-1.13.3", "httparse-1.10.1", "serde_json-1.0.154",
]
# Doctests known to fail under blaze, by crate and libtest's name: each has to fail, and one
# that passes or is gone is taken out.
KNOWN: dict[str, frozenset[str]] = {}
REFUSED = "blaze did not build the example: "


def doctest_report(stdout):
    """What a run of doctests printed, without what differs from run to run: the order the
    tests finished in, how long they took, and the numbers of the threads."""
    out = re.sub(r"finished in [0-9.]+s", "finished", stdout)
    # rustdoc builds the examples of an edition 2024 crate as one program and says how long
    # that took; blaze builds each alone.
    out = re.sub(r"\nall doctests ran in [0-9.]+s; merged doctests compilation took [0-9.]+s\n", "\n", out)
    out = re.sub(r"thread '(.*?)' \(\d+\) panicked", r"thread '\1' panicked", out)
    blocks = re.split(r"\n(?=---- )", out)
    # The last failure is followed by the names of all of them and the result.
    ends = []
    if len(blocks) > 1:
        blocks[-1], _, end = blocks[-1].partition("\n\nfailures:\n")
        ends = sorted(l for l in end.splitlines() if l.startswith("    ")) + [l for l in end.splitlines() if not l.startswith("    ")]
    lines = blocks[0].splitlines()
    tests = sorted(l for l in lines if l.startswith("test ") and not l.startswith("test result"))
    rest = [l for l in lines if l not in tests]
    return tests + rest + sorted(b.strip("\n") for b in blocks[1:]) + ends


def outcomes(report):
    """Each doctest of a `doctest_report` by libtest's name, with how it ended (`ok`, `FAILED`, `ignored`)."""
    found = {}
    for line in report:
        m = re.fullmatch(r"test (.+) \.\.\. (\S+)", line)
        if m and not line.startswith("test result"):
            found[m[1]] = m[2]
    return found


def judge_doctests(run, known=frozenset()):
    """What one `blaze test --doc` run, (exit status, [`doctest_report`], standard error), comes
    to by its own results: ("pass", what it ran), ("refused", ...) when every doctest blaze
    built passed and some it did not build, or ("WRONG", what is wrong)."""
    code, report, stderr = run
    refused = {l[5:l.index(" stdout ----")] for l in report if l.startswith("---- ") and f" stdout ----\n{REFUSED}" in l}
    ended = outcomes(report)
    failed = {name for name, how in ended.items() if how == "FAILED"} - refused
    wrong = []
    if new := sorted(failed - known):
        wrong.append(f"{len(new)} failed: {', '.join(new[:4])}")
    if passed := sorted(name for name in known if ended.get(name) == "ok"):
        wrong.append(f"known to fail, and passed: {', '.join(passed[:4])}")
    if gone := sorted(known - set(ended)):
        wrong.append(f"known to fail, and did not run: {', '.join(gone[:4])}")
    if not ended:
        wrong.append(f"no doctest ran: {stderr.strip()[-200:]}")
    if code != 0 and not failed and not refused:
        wrong.append(f"exited {code} with no doctest failed: {stderr.strip()[-200:]}")
    built = re.findall(r"doctests: (\d+) built by blaze, (\d+) not built", stderr)
    if not built:
        wrong.append("blaze did not say what it built for the doctests")
    if wrong:
        return "WRONG", "; ".join(wrong)[:300]
    said = f"{len(ended)} doctests, {sum(int(n) for n, _ in built)} built by blaze, {len(refused)} not built"
    return ("refused" if refused else "pass"), said


def tested(crate, target_dir):
    """How the crate's doctests ended, what they printed, and what was said beside."""
    path = os.path.dirname(BLAZE) + os.pathsep + os.environ["PATH"]
    run = subprocess.run(["blaze", "test", "--doc"], cwd=crate, capture_output=True, text=True,
                         timeout=3600, env={**os.environ, "PATH": path, "CARGO_TARGET_DIR": target_dir})
    return run.returncode, doctest_report(run.stdout), run.stderr


def row(spec):
    """The crate's line: its verdict and what there is to say of it."""
    subprocess.run(["sh", os.path.join(HERE, "vendor.sh"), spec], check=True)
    crate = os.path.join(HERE, "vendor", spec.rsplit("-", 1)[0])
    with tempfile.TemporaryDirectory(prefix="blaze-doctests-") as scratch:
        got = tested(crate, os.path.join(scratch, "blaze"))
        # The second run has the programs of the first, and the store.
        again = tested(crate, os.path.join(scratch, "blaze"))
    verdict = judge_doctests(got, KNOWN.get(spec.rsplit("-", 1)[0], frozenset()))
    if verdict[0] != "WRONG" and got[:2] != again[:2]:
        lines = [f"first `{a}`, second `{b}`" for a, b in zip(got[1], again[1]) if a != b]
        return "WRONG", "the second run differs: " + (lines[0] if lines else f"exit {got[0]}, then {again[0]}")[:300]
    return verdict


def main():
    args = [a for a in sys.argv[1:] if a != "--few"]
    counts = {"pass": 0, "refused": 0, "WRONG": 0}
    for spec in args or (CRATES[:3] if "--few" in sys.argv else CRATES):
        verdict, said = row(spec)
        counts[verdict] += 1
        print(f"{spec:28} {verdict:8} {said}", flush=True)
    print(f"{counts['pass']} pass, {counts['refused']} refused, {counts['WRONG']} WRONG")
    sys.exit(1 if counts["WRONG"] else 0)


if __name__ == "__main__":
    main()
