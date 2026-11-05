#!/usr/bin/env python3
"""linkbench.py ARGS LINKERS WHAT [N]: times variations of one link line, run without the compiler driver.

ARGS holds the linker's arguments one per line, with blaze's `*.patch.o` right before its whole object
(recorded by putting a script that writes its arguments in place of `ld` under `cc -B`).
LINKERS is `name=/path/to/ld,...`. WHAT is one of:

  base     the line as it is, and whether the binary runs
  flags    the line with one flag added or dropped at a time
  start    the linker started for `--version`
  prelink  everything but the patch linked to one relocatable object, then the patch against it
  shared   everything but the patch linked to a shared object, then the patch against it

Each line printed has every run in ms, so that nothing is a mean of unseen numbers.
"""
import os, statistics, subprocess, sys, time

FLAGS = [[], ["--build-id=none"], ["--strip-debug"], ["--strip-debug", "--build-id=none"], ["--strip-all", "--build-id=none"],
         ["--no-pie"], ["--no-eh-frame-hdr"], ["--gc-sections"], ["--threads=16"], ["--no-threads"]]


def timed(command, n):
    runs = []
    for _ in range(n):
        began = time.perf_counter()
        ran = subprocess.run(command, capture_output=True, text=True)
        runs.append(round((time.perf_counter() - began) * 1000))
    return runs, ran


def show(what, runs, ran, out):
    size = os.path.getsize(out) if out and ran.returncode == 0 and os.path.exists(out) else None
    error = "" if ran.returncode == 0 else "  FAILED: " + (ran.stderr.strip().splitlines() or ["?"])[0][:160]
    print(f"{what}: runs_ms={runs} median={statistics.median(runs):.0f} rc={ran.returncode} size={size}{error}", flush=True)
    return ran.returncode == 0


def works(binary):
    """Whether the binary linked is dx: its `doctor --help` says so."""
    try:
        ran = subprocess.run([binary, "doctor", "--help"], capture_output=True, text=True, timeout=30)
    except Exception as error:
        return f"did not run: {error}"
    return f"exit {ran.returncode}, prints {ran.stdout.splitlines()[:1]}"


def main():
    args_path, linkers, what, *rest = sys.argv[1:]
    n = int(rest[0]) if rest else 5
    work = os.path.dirname(os.path.abspath(args_path))
    line = [a for a in open(args_path).read().split("\n") if not a.startswith("-plugin")]
    # What rustc built, as opposed to the C runtime's objects around it.
    is_input = lambda a: a.endswith((".rcgu.o", ".rlib"))
    patch = next(i for i, a in enumerate(line) if a.endswith(".patch.o"))
    for name, ld in (pair.split("=") for pair in linkers.split(",")):
        out = os.path.join(work, f"linkbench-{name}")
        with_out = lambda path, args=line: [path if i and args[i - 1] == "-o" else a for i, a in enumerate(args)]
        if what == "start":
            show(f"{name} --version", *timed([ld, "--version"], n), None)
        elif what == "base":
            show(name, *timed([ld, *with_out(out)], n), out)
            print(f"  {works(out)}")
        elif what == "flags":
            for flags in FLAGS:
                show(f"{name} {' '.join(flags)}", *timed([ld, *with_out(out), *flags], n), out)
        elif what == "prelink":
            # Everything the line names but the patch, in its order; archives give what is referred to.
            inputs = [line[patch + 1]] + [a for a in line[patch + 2:] if is_input(a)]
            whole = os.path.join(work, f"linkbench-{name}.prelink.o")
            made = show(f"{name} -r", *timed([ld, "-r", "--allow-multiple-definition", "-o", whole, *inputs], 1), whole)
            if not (made and os.path.exists(whole)):
                continue
            final = line[:patch + 1] + [whole] + line[patch + 2:]
            if show(f"{name} patch + prelinked object + the line", *timed([ld, *with_out(out, final)], n), out):
                print(f"  {works(out)}")
            alone = line[:patch + 1] + [whole] + [a for a in line[patch + 2:] if not is_input(a)]
            if show(f"{name} patch + prelinked object alone", *timed([ld, *with_out(out, alone)], n), out):
                print(f"  {works(out)}")
        elif what == "shared":
            shared = os.path.join(work, f"liblinkbench-{name}.so")
            rest_of = [a for a in line[:patch] + line[patch + 1:] if a != "-pie" and not a.endswith(("crt1.o", "Scrt1.o"))]
            if not show(f"{name} -shared", *timed([ld, "-shared", *with_out(shared, rest_of)], 1), shared):
                continue
            final = line[:patch + 1] + [shared, "-rpath", work] + [a for a in line[patch + 2:] if not is_input(a)]
            if show(f"{name} patch against the shared object", *timed([ld, *with_out(out, final)], n), out):
                print(f"  {works(out)}")


if __name__ == "__main__":
    main()
