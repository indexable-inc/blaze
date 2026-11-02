#!/usr/bin/env python3
"""realdebug.py: what a debugger, a panic, perf and addr2line are told by a program
`blaze` built of a real project.

    realdebug.py [PROJECT...] [--keep]

Each project is cloned as blaze_cli.py has it, and copied. A panic that an environment
variable asks for is written into the copy, and `blaze` builds it cold, waits for the
daemon, and builds it again after a comment is added, so that the program looked at is the
one the daemon linked. Then, in the dev profile:
  gdb       stops at each place, in a function of blaze's, and has its arguments and locals
            with values, none of them optimized out (v2 compiles the dev profile with Cranelift)
  panic     each panic has a frame at the line the panic was written into, by file:line
  perf      `perf record --call-graph dwarf`, then `perf report` names the symbols and
            `perf script` the lines of the frames in blaze's functions
  addr2line every function of blaze's has a name and a line at its first address
  dwarfdump `llvm-dwarfdump --verify` has nothing to say of blaze's units
"""
import os, re, shutil, signal, subprocess, sys, tempfile, time, urllib.request, http.client

import blaze_cli
from blaze_cli import PROJECTS, Side, base_env, fetch, how_built, rewrite, wait_warm
from dwarf import DWARFDUMP, OPTIMIZED_OUT, Tally, held, own_ranges, panics, units, unit_of

PANIC = 'if std::env::var_os("REALDEBUG_PANIC").is_some() { panic!("asked for"); }'

# Where to stop is the line of a piece of text, so that it is a line of the tag's source.
CHECKS = {
    "ripgrep": dict(
        binary="rg",
        run=["-n", "--no-heading", "-j1", "fn.\\w+", "."],
        panic=("crates/core/search.rs", "        use self::PatternMatcher::*;\n\n        let (searcher, printer)",
               "        use self::PatternMatcher::*;\n        " + PANIC + "\n        let (searcher, printer)"),
        places=[("crates/core/main.rs", "let mut searched = false;"),
                ("crates/core/main.rs", "let mut searcher = args.search_worker("),
                ("crates/core/search.rs", "RustRegex(ref m) => search_path(m, searcher, printer, path),"),
                ("crates/core/search.rs", "searcher.search_path(&matcher, path, &mut sink)?;"),
                ("crates/searcher/src/searcher/mod.rs", "let path = path.as_ref();"),
                ("crates/ignore/src/walk.rs", "let follow_links = self.follow_links;")],
    ),
    "axum": dict(
        binary="example-hello-world",
        build=["-p", "example-hello-world"], serves="http://127.0.0.1:3000/",
        panic=("examples/hello-world/src/main.rs", '    Html("<h1>Hello, World!</h1>")',
               "    " + PANIC + '\n    Html("<h1>Hello, World!</h1>")'),
        places=[("examples/hello-world/src/main.rs", 'println!("listening on {}"'),
                ("examples/hello-world/src/main.rs", "axum::serve(listener, app).await.unwrap();"),
                ("examples/hello-world/src/main.rs", PANIC),
                ("axum/src/serve/mod.rs", "let (_close_tx, close_rx) = watch::channel(());"),
                ("axum/src/serve/mod.rs", "let hyper_service = TowerToHyperService::new(tower_service);"),
                ("axum/src/routing/mod.rs", "let (req, state) = match self.inner.path_router.call_with_state(req, state) {")],
    ),
}


def line_of(ws, file, text):
    source = open(os.path.join(ws, file)).read()
    assert text in source, f"{text!r} is not in {file}"
    return source[:source.index(text)].count("\n") + 1


def asked(url, times=1):
    """Asks a server that is starting for `url`, `times` over. The result is how often it answered."""
    answered = 0
    for _ in range(600):
        try:
            urllib.request.urlopen(url, timeout=5).read()
            answered += 1
        except (urllib.error.HTTPError, http.client.RemoteDisconnected):
            # A handler that panicked has answered too: its connection is closed.
            answered += 1
        except OSError:
            time.sleep(0.1)
        if answered == times:
            break
    return answered


def driven(argv, cwd, env, serves, times=1):
    """Runs the program to its end, or a server until it has answered. The result is what it wrote to stderr."""
    if not serves:
        return subprocess.run(argv, cwd=cwd, env=env, capture_output=True, text=True, errors="replace", timeout=900).stderr
    # To a file: a pipe nobody reads fills, and the server stops at its next panic.
    with tempfile.TemporaryFile(mode="w+", errors="replace") as said:
        server = subprocess.Popen(argv, cwd=cwd, env=env, stdout=subprocess.DEVNULL, stderr=said, start_new_session=True)
        try:
            asked(serves, times)
        finally:
            # perf writes what it recorded when it is interrupted, and takes the server with it.
            os.killpg(server.pid, signal.SIGINT)
            try:
                server.wait(timeout=60)
            except subprocess.TimeoutExpired:
                os.killpg(server.pid, signal.SIGKILL)
                server.wait()
        said.seek(0)
        return said.read()


def told(binary, ws, command):
    """What gdb says to one command about a program it does not run."""
    return subprocess.run(["gdb", "-q", "-nx", "-batch", "-ex", command, binary],
                          capture_output=True, text=True, errors="replace", cwd=ws, timeout=90).stdout


def stopped(binary, ws, place, run, serves):
    """What gdb says where it stops at `place`: the function, and each argument and local by name."""
    commands = ["set pagination off", "set confirm off", "set startup-with-shell off", "set print elements 40",
                "set print address off", f"break {place}"]
    if serves:
        commands.append(f"shell sh -c '(sleep 3; curl -s -m 20 {serves} >/dev/null) &'")
    commands += ["run " + " ".join(run), "echo <where>\\n", "bt 1", "echo <pc>\\n", "print/x $pc", "echo <mapped>\\n",
                 "info proc mappings", "echo <args>\\n", "info args", "echo <locals>\\n", "info locals", "echo <end>\\n", "kill"]
    try:
        out = subprocess.run(["gdb", "-q", "-nx", "-batch", *[x for c in commands for x in ("-ex", c)], binary],
                             capture_output=True, text=True, errors="replace", cwd=ws, timeout=90).stdout
    except subprocess.TimeoutExpired:
        return None
    line = place.rsplit(":", 1)[1]
    if not re.search(rf"Breakpoint 1(?:\.\d+)?, .*?:{line}\n", out, re.S) or "<end>" not in out:
        return None
    part = lambda a, b: out.split(a)[1].split(b)[0]
    name = re.search(r"^#0\s+(?:0x\w+ in )?(.*?) \(", part("<where>", "<pc>"), re.M)
    pc = re.search(r"= (0x[0-9a-f]+)", part("<pc>", "<mapped>"))
    base = re.search(r"^\s*(0x[0-9a-f]+)\s+0x[0-9a-f]+\s+0x[0-9a-f]+\s+0x0\s", part("<mapped>", "<args>"), re.M)
    values = {}
    for text in (part("<args>", "<locals>"), part("<locals>", "<end>")):
        for found in re.finditer(r"^(\w+) = (.*(?:\n(?!\w+ = ).*)*)", text, re.M):
            # The innermost block's is the first one listed of a name bound more than once.
            values.setdefault(found.group(1), found.group(2).strip())
    return dict(name=name.group(1) if name else "?", values=values,
                at=int(pc.group(1), 16) - int(base.group(1), 16) if pc and base else None)


def debugger(tally, what, binary, ws, spec):
    run, serves = spec.get("run", []), spec.get("serves")
    for file, text in spec["places"]:
        place = f"{file}:{line_of(ws, file, text)}"
        said = f"{what}: gdb at {os.path.basename(file)}:{place.rsplit(':', 1)[1]}"
        ours = stopped(binary, ws, place, run, serves)
        if not ours:
            tally.say(said, "NOT STOPPED")
            continue
        producer = unit_of(binary, ours["at"]) if ours["at"] is not None else "nothing"
        if not producer.startswith("blaze"):
            tally.say(said, "NOT BLAZE'S", f"the code is of {producer}")
            continue
        # What an `async` body is resumed with is of no name the source has.
        names = [name for name in ours["values"] if name != "_task_context"]
        gone = [name for name in names if OPTIMIZED_OUT in ours["values"][name]]
        verdict = "OPTIMIZED OUT" if gone else "stopped"
        more = f"in {ours['name'][:50]}: {len(names)} names, {len(gone)} optimized out"
        if gone and verdict.isupper():
            more += f"; {gone[0]}"
        tally.say(said, verdict, more)


def backtrace(tally, what, binary, ws, spec):
    env = dict(os.environ, RUST_BACKTRACE="1", REALDEBUG_PANIC="1")
    ours = panics(driven([binary, *spec.get("run", [])], ws, env, spec.get("serves")))
    file = spec["panic"][0]
    site = f"{file}:{line_of(ws, file, PANIC)}"
    held(tally, f"{what}: frames of each panic", ours, [[(None, site)]] * max(len(ours), 1))


def profiler(tally, what, binary, ws, spec):
    perf = shutil.which("perf")
    data = os.path.join(tempfile.mkdtemp(prefix="blaze-realdebug-perf-"), "perf.data")
    if perf:
        driven([perf, "record", "-o", data, "-F", "2000", "--call-graph", "dwarf,32768", binary, *spec.get("run", [])],
               ws, dict(os.environ), spec.get("serves"), times=300)
    if not perf or not os.path.exists(data):
        return tally.say(f"{what}: perf", "not run", "no perf here, or it may not sample: see kernel.perf_event_paranoid")
    # A function is blaze's when a unit of blaze's has its first address.
    starts = {start for start, _ in own_ranges(binary)}
    table = subprocess.run(["nm", "--defined-only", binary], capture_output=True, text=True, errors="replace").stdout
    mine = {name for at, name in re.findall(r"^([0-9a-f]+) \w (\S+)$", table, re.M) if int(at, 16) in starts}
    script = subprocess.run([perf, "script", "--no-demangle", "-i", data, "-F", "ip,sym,srcline"],
                            capture_output=True, text=True, errors="replace").stdout
    frames = [(name, line) for name, line in re.findall(r"^[ \t]+[0-9a-f]+ (\S+)\n[ \t]+(\S+)(?: \(inlined\))?$", script, re.M) if name in mine]
    lost = [name for name, line in frames if not re.search(r"\.rs:\d+$", line)]
    report = subprocess.run([perf, "report", "--no-demangle", "-i", data, "--stdio", "--no-children", "--sort", "dso,symbol", "-g", "none"],
                            capture_output=True, text=True, errors="replace").stdout
    rows = [row.split("[.]")[1].split()[0] for row in report.splitlines() if os.path.basename(binary)[:15] in row and "[.]" in row]
    unnamed = [row for row in rows if re.fullmatch(r"0x[0-9a-f]+", row)]
    ours = [row for row in rows if row in mine]
    verdict = "NO SAMPLES" if not frames or not ours else "NO LINE" if lost else "NO SYMBOL" if unnamed else "named"
    tally.say(f"{what}: perf record, report and script", verdict,
              f"report: {len(rows)} symbols of the program, {len(ours)} of blaze's functions, {len(unnamed)} without a name; "
              f"script: {len(frames)} frames in blaze's functions, {len(lost)} without a line" + (f": {lost[0][:60]}" if lost else ""))


def addr2line(tally, what, binary):
    starts = sorted(start for start, _ in own_ranges(binary))
    said = subprocess.run(["addr2line", "-f", "-e", binary], input="\n".join(f"{it:#x}" for it in starts),
                          capture_output=True, text=True, errors="replace").stdout.splitlines()
    pairs = list(zip(said[::2], said[1::2]))
    unnamed = [at for at, (name, _) in zip(starts, pairs) if name.startswith("??")]
    unlined = [at for at, (_, line) in zip(starts, pairs) if not re.search(r"\.rs:\d+", line)]
    verdict = "NOT ASKED" if len(pairs) != len(starts) or not starts else "NO NAME" if unnamed else "NO LINE" if unlined else "resolved"
    tally.say(f"{what}: addr2line at each function", verdict,
              f"{len(starts)} functions of blaze's, {len(unnamed)} without a name, {len(unlined)} without a line"
              + (f": {(unnamed + unlined)[0]:#x}" if unnamed or unlined else ""))


def verify(tally, what, binary):
    """`llvm-dwarfdump --verify`, for the units of blaze's: rustc's own have functions the linker
    left out, which it finds to overlap."""
    said = subprocess.run([DWARFDUMP, "--verify", binary], capture_output=True, text=True, errors="replace").stdout
    heads = units(binary)
    count = sum(bool(re.search(r'DW_AT_producer\s+\("blaze[ "]', unit)) for unit in heads)
    spans = []
    listed = subprocess.run([DWARFDUMP, "--debug-info", "--show-children=0", "--recurse-depth=0", binary],
                            capture_output=True, text=True, errors="replace").stdout
    for unit in re.finditer(r"^(0x[0-9a-f]+): Compile Unit: length = (0x[0-9a-f]+).*?(?=^0x[0-9a-f]+: Compile Unit|\Z)", listed, re.S | re.M):
        if re.search(r'DW_AT_producer\s+\("blaze[ "]', unit.group(0)):
            spans.append((int(unit.group(1), 16), int(unit.group(1), 16) + int(unit.group(2), 16)))
    errors = [it for it in re.split(r"^error: ", said, flags=re.M)[1:] if "occurred" not in it.splitlines()[0] and not it.startswith("Aggregated")]
    ours = [it for it in errors
            if any(low <= int(at, 16) < high for at in re.findall(r"^(0x[0-9a-f]+): DW_TAG", it, re.M)[:2] for low, high in spans)
            or not re.search(r"^0x[0-9a-f]+: DW_TAG", it, re.M)]
    tally.say(f"{what}: llvm-dwarfdump --verify", "INVALID" if ours else "sound" if count else "NO UNIT",
              f"{count} units of blaze's with {len(ours)} errors, {len(errors) - len(ours)} in rustc's units"
              + (f": {ours[0].splitlines()[0][:70]}" if ours else ""))


def project(tally, name, work):
    spec, check = PROJECTS[name], CHECKS[name]
    ws = os.path.realpath(os.path.join(work, name))
    shutil.copytree(fetch(name, spec), ws, symlinks=True, ignore=shutil.ignore_patterns("target"))
    rewrite(os.path.join(ws, check["panic"][0]), *check["panic"][1:])
    env = base_env(os.path.join(work, "home"))
    ours = Side(ws, env, True)
    build = check.get("build", [])
    ours.cargo("stop")
    took, ran = ours.cargo("build", *build)
    if ran.returncode != 0:
        return tally.say(f"{name}: built", "NOT BUILT", ran.stderr[-200:])
    _, states = wait_warm(ours, ws)
    with open(os.path.join(ws, check["panic"][0]), "a") as source:
        source.write("// built again\n")
    took, ran = ours.cargo("build", *build)
    built = how_built(ran.stderr)
    tally.say(f"{name}: built", "by the daemon" if built == "daemon" else "NOT BY THE DAEMON", f"{built}; {'; '.join(states)[:80]}; {took:.2f}s")
    binary = os.path.join(ws, "target", "debug", check["binary"])
    verify(tally, name, binary)
    debugger(tally, name, binary, ws, check)
    backtrace(tally, name, binary, ws, check)
    profiler(tally, name, binary, ws, check)
    addr2line(tally, name, binary)
    ours.cargo("stop")


def main():
    names = [a for a in sys.argv[1:] if not a.startswith("--")] or list(CHECKS)
    work = tempfile.mkdtemp(prefix="blaze-realdebug-")
    tally = Tally()
    try:
        for name in names:
            project(tally, name, work)
    finally:
        if "--keep" in sys.argv:
            print(f"kept: {work}")
        else:
            shutil.rmtree(work, ignore_errors=True)
    print(f"{'wrong: ' + str(tally.wrong) if tally.wrong else 'all right'}")
    sys.exit(1 if tally.wrong else 0)


if __name__ == "__main__":
    main()
