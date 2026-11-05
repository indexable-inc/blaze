#!/usr/bin/env python3
"""heapfold.py PROFILE BINARY > FOLDED: a heap profile the daemon wrote, as `jeprof --collapsed`
makes of it, on macOS, where jeprof can't.

A profile has the addresses of the code that allocated. jemalloc does not write on macOS where
the program was loaded, so jeprof names them as if it were not moved and every name is wrong. The
daemon writes the address it was loaded at next to the profile (`build.load` beside
`build.prof`); this names each address with `atos` at that address, demangles with `rustfilt`
(`cargo install rustfilt`) and scales each stack's sampled bytes to the bytes it stands for, as
jeprof does. The result is what `heap.py` reads. On Linux use `jeprof --collapsed BINARY PROFILE`.
"""
import math, os, re, shutil, subprocess, sys


def stacks(profile):
    """Each stack of the profile, leaf first, with the bytes its samples stand for."""
    lines = open(profile).read().splitlines()
    rate = int(lines[0].split("/")[1])
    for at, line in enumerate(lines):
        if not line.startswith("@"):
            continue
        count, size = map(int, re.match(r"\s*t\*: (\d+): (\d+)", lines[at + 1]).groups())
        if count:
            # A sample is taken about every `rate` bytes: an allocation of `size / count`
            # bytes is sampled with the chance 1 - e^(-size/rate).
            yield [int(it, 16) for it in line[1:].split()], size / (1 - math.exp(-size / count / rate))


def names(binary, load, addresses):
    """What `atos` calls the function at each return address, demangled."""
    found = {}
    addresses = sorted(addresses)
    for at in range(0, len(addresses), 4000):
        chunk = addresses[at:at + 4000]
        # A return address is after the call: the call is the instruction before it.
        said = subprocess.run(["atos", "-o", binary, "-l", hex(load), *(hex(it - 1) for it in chunk)],
                              capture_output=True, text=True, check=True).stdout.splitlines()
        for address, name in zip(chunk, said):
            found[address] = re.sub(r" \((in [^)]*|[^)]*:\d+)\)", "", name)
    rustfilt = shutil.which("rustfilt") or os.path.expanduser("~/.cargo/bin/rustfilt")
    keys = list(found)
    plain = subprocess.run([rustfilt], input="\n".join(found[it] for it in keys), capture_output=True,
                           text=True, check=True).stdout.splitlines()
    return {key: name.replace(";", ",") for key, name in zip(keys, plain)}


def main():
    profile, binary = sys.argv[1], sys.argv[2]
    load = int(open(os.path.splitext(profile)[0] + ".load").read(), 16)
    found = list(stacks(profile))
    named = names(binary, load, {it for stack, _ in found for it in stack if it >= load})
    for stack, size in found:
        print(";".join(named.get(it, hex(it)) for it in reversed(stack)), int(size))


if __name__ == "__main__":
    main()
