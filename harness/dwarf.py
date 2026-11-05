"""dwarf.py: what a binary's debug info and a run's backtraces say, read for realdebug.py.

`Tally` keeps what was checked and what was wrong; `held` checks a run's panics against the places
the source says each passes through; `units`, `own_ranges` and `unit_of` read a binary's DWARF
with the pinned LLVM's `llvm-dwarfdump`.
"""
import os, re, shutil, subprocess

WINDOWS = os.name == "nt"
# The pinned LLVM's, on PATH in the flake's dev shell and in CI.
DWARFDUMP = shutil.which("llvm-dwarfdump") or "llvm-dwarfdump"
# What rustc calls the code that drops a coroutine, which blaze has the coroutine itself do.
DROP_GLUE = re.compile(r"^core::ptr::drop_glue::<(.*)>$")
# The same in CodeView, where rustc calls a coroutine `async_fn$0` and what it keeps `async_fn_env$0`.
COROUTINE = re.compile(r"\b(?:async_gen|async|gen)_(?:fn|block)\$(\d+)")
DROP_GLUE_CODEVIEW = re.compile(r"^core::ptr::drop_glue<enum2\$<(.*)::(?:async_gen|async|gen)_(?:fn|block)_env\$(\d+)> >$")

OPTIMIZED_OUT = "<optimized out>"


def panics(stderr):
    """The panics a run printed: where each says it was and what it said, and its frames as
    [name, place]."""
    found, lines = [], stderr.splitlines()
    for at, line in enumerate(lines):
        if "panicked at" in line:
            said = lines[at + 1].strip() if at + 1 < len(lines) else ""
            found.append((line.split("panicked at")[1].strip() + " " + said, []))
        elif found and (frame := re.match(r"\s+\d+: (.*)", line)):
            found[-1][1].append([frame.group(1).strip(), None])
        elif found and found[-1][1] and (place := re.match(r"\s+at (.*)", line)):
            found[-1][1][-1][1] = place.group(1).strip()
    return found


def last_part(name):
    """What a function is called without what it is in and what it is given: `pick` of both
    `sline::pick::<u8>`, which is how a symbol of rustc's reads, and `sline::pick<u8>`, which
    is how rustc names it in CodeView. A closure keeps what it is a closure of."""
    if glue := DROP_GLUE_CODEVIEW.match(name):
        name = f"{glue.group(1)}::closure${glue.group(2)}"
    name = COROUTINE.sub(r"closure$\1", name)
    parts, depth, start, at = [], 0, 0, 0
    while at < len(name):
        if name[at] in "<({[":
            depth += 1
        elif name[at] in ")}]" or (name[at] == ">" and name[at - 1] != "-"):
            depth -= 1
        elif depth == 0 and name.startswith("::", at):
            parts.append(name[start:at])
            start, at = at + 2, at + 1
        at += 1
    parts = [re.sub(r"^\{closure#(\d+)\}$", r"closure$\1", part) for part in parts + [name[start:]]]
    named = [part.split("<")[0] for part in parts if part and not part.startswith("<")] or [name]
    keep = len(named) - 1
    while keep > 0 and named[keep].startswith("closure$"):
        keep -= 1
    return "::".join(named[keep:])


def own(frames):
    """The frames of the workspace's own files, by function and file:line."""
    kept = []
    for name, place in frames:
        place = place.replace("\\", "/") if place else ""
        if not place.startswith("./"):
            continue
        if WINDOWS:
            # A frame has no column there.
            kept.append((last_part(name), place))
            continue
        glue = DROP_GLUE.match(name)
        kept.append((glue.group(1) if glue else name, place.rsplit(":", 1)[0]))
    return kept


# What is known to be wrong, and why.
OPEN = set()


class Tally:
    def __init__(self):
        self.wrong, self.open, self.lines = 0, 0, []

    def say(self, what, verdict, more=""):
        if what in OPEN and verdict.isupper():
            more, verdict = f"{verdict}: {more}", "open"
        elif what in OPEN:
            more, verdict = f"{verdict}: take it out of OPEN in harness/dwarf.py", "SAME NOW"
        self.wrong += verdict.isupper()
        self.open += verdict == "open"
        print(f"{what:58} {verdict:14} {more}", flush=True)


def held(tally, what, ours, sites):
    """Holds the frames of each panic of blaze's binary to the places the source says it passes
    through. `sites` has, for each panic, its (function, file:line) from where it panics out, in
    order, the function as its symbol names it or None; only frames of the workspace's own files
    are looked at, and others may be between them. Every place has to have its frame: the
    programs are of the dev profile, which v2 compiles with Cranelift, so no frame is optimized
    away."""
    if len(ours) != len(sites):
        return tally.say(what, "NOT THE PANICS", f"{len(ours)} for {len(sites)}")
    differ, frames, shown = 0, 0, ""
    for (at, got), want in zip(ours, sites):
        places = [place.removeprefix("./") for _, place in own(got)]
        frames += len(places)
        missing, start = [], 0
        for name, place in want:
            if place in places[start:]:
                start += places[start:].index(place) + 1
            else:
                missing.append((name, place))
        if not missing:
            continue
        differ += 1
        shown = shown or f"at {at}: {[it[1] for it in missing][:2]} not in {places[:4]}"
    if not frames:
        return tally.say(what, "NO FRAMES")
    more = f"{len(ours)} panics, {frames} frames"
    tally.say(what, "NOT WHERE THE SOURCE SAYS" if differ else "where the source says",
              more + (f"; {differ} differ, first {shown}" if differ else ""))


def dumped(binary, *what):
    """What `llvm-dwarfdump` says of the program, read from the files beside it too where a
    unit's entries are kept apart (split DWARF), with their addresses as the program has them."""
    return subprocess.run([DWARFDUMP, "--dwo", *what, binary], capture_output=True, text=True).stdout


def units(binary):
    """What each unit's own entry says. A unit kept apart is two entries, the one left in the
    program with the addresses and the one beside it with the producer: they are put together."""
    heads = [it.split("\n\n")[0] for it in dumped(binary, "--debug-info", "--show-children=0").split("DW_TAG_compile_unit")[1:]]
    ident = lambda head: (re.search(r"DW_AT_GNU_dwo_id\s+\((\w+)\)", head) or [None, None])[1]
    left = {ident(head): head for head in heads if ident(head) and "DW_AT_producer" not in head}
    return [head + left.get(ident(head), "") for head in heads if "DW_AT_producer" in head or ident(head) not in left]


def own_ranges(binary):
    """Where the code of each function of blaze's is, as its units say."""
    ours = set()
    for unit in units(binary):
        if re.search(r'DW_AT_producer\s+\("blaze[ "]', unit):
            ours |= {(int(a, 16), int(b, 16)) for a, b in re.findall(r"\[0x([0-9a-f]+), 0x([0-9a-f]+)\)", unit)}
    return ours


def unit_of(binary, address):
    producer = re.search(r'DW_AT_producer\s+\("([^"]*)"\)', dumped(binary, f"--lookup={address:#x}"))
    return producer.group(1) if producer else "nothing"
