#!/usr/bin/env python3
"""realbench.py: build times of real projects, `blaze` against plain cargo (issue #7).

    realbench.py [PROJECT...] [--runs N] [--work DIR] [--parts cold,noop,edits] [--pause S]

Projects are cloned at the tags `blaze_cli.py` pins, into `BLAZE_PROJECTS`
(~/blaze-projects), and copied into WORK/<project>/a; the clone is never written to.
Cargo builds into WORK/<project>/t-cargo and blaze into WORK/<project>/t-blaze, both from
the same checkout, so an edit is seen by both. Every step runs N times (default 5) and the
report gives the median, min and max.

  cold        per run, in this order: `cargo build` with an empty target directory;
              `blaze build` with an empty target directory, no daemon and an empty
              store; the same with the store filled beforehand by a blaze build of the same
              commit in another checkout (WORK/<project>/b). For blaze two times: until
              the command returns (cargo's build through the wrapper) and until the daemon
              says the checkout is ready (loaded, and every program built once by it).
  noop        `cargo build` and `blaze build` with nothing changed.
  edits       main: a statement added to the body of the program's own `main`;
              lib: the same in a function of a library of the workspace the program uses;
              newfn: a new function in the program's crate, called from `main`.
              Each run writes a different statement, so every run is a real change. Per
              run, `blaze build` then `cargo build`; then both programs are run with
              `--version` and with a small real command line, and their stdout, stderr and
              exit status must be the same.

`--pause S` waits S seconds once the daemon is ready, before the no-op and the edits, as a
person would before the first edit: what the daemon does by itself once ready is then over.

Every warm `blaze build` (no-op and edits) runs with BLAZE_PROFILE, and the row keeps
where the time of its median run went: the client's breakdown of its wall time into steps of
the client, the daemon and the worker, plus the time outside the client (cargo, process
start). `--table FILE` prints the results table and that breakdown table.

Results go to WORK/realbench.json (rows replaced per project) and a table on stdout.
"""
import asyncio, json, os, re, shutil, statistics, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
BLAZE = os.environ.get("BLAZE_BIN") or os.path.join(ROOT, "target/release/blaze-engine")
TOOLCHAIN = open(os.path.join(HERE, "toolchain")).read().strip()
SOURCES = os.environ.get("BLAZE_PROJECTS") or os.path.expanduser("~/blaze-projects")
STEP_SECS = 3600


def stmt(r, indent):
    return f"\n{indent}let _ = std::hint::black_box({r}u64);"


def added(r, indent):
    return f"\n{indent}let _ = std::hint::black_box(blaze_added_{r}({r}));"


def added_fn(r):
    return f"\n#[inline(never)]\nfn blaze_added_{r}(x: u64) -> u64 {{\n    x.wrapping_mul(31) ^ {r}\n}}\n"


# Tags and revisions are the ones of `blaze_cli.py`.
PROJECTS = {
    "ripgrep": dict(
        repo="BurntSushi/ripgrep", tag="15.1.0", rev="af60c2de9d85e7f3d81c78601669468cf02dabab", bin="rg",
        run=["-n", "--no-heading", "--sort", "path", "fn main", "crates/core/main.rs", "build.rs"],
        main=("crates/core/main.rs", "fn main() -> ExitCode {", "    "),
        lib=("crates/matcher/src/lib.rs", "    pub fn is_suffix(&self, slice: &[u8]) -> bool {", "        "),
    ),
    "just": dict(
        repo="casey/just", tag="1.43.0", rev="b7e7c46495b34e9407982ffbbb25eb2808ec40b7", bin="just",
        run=["--list", "--unsorted"],
        main=("src/main.rs", "fn main() {", "  "),
        lib=("src/lexer.rs", "  pub(crate) fn lex(path: &'src Path, src: &'src str) -> CompileResult<'src, Vec<Token<'src>>> {", "    "),
    ),
    "nushell": dict(
        repo="nushell/nushell", tag="0.108.0", rev="da141be11b3acaa55b39321e5f9651d02aa2ed6f", bin="nu",
        run=["-n", "-c", "[3 1 2] | sort | str join ','"],
        main=("src/main.rs", "fn main() -> Result<()> {", "    "),
        lib=("crates/nu-protocol/src/value/mod.rs", "    pub fn get_type(&self) -> Type {", "        "),
    ),
}


def edit_text(kind, spec, text, r):
    """The text of the edited file for run `r` of edit `kind`."""
    file, anchor, indent = spec["lib" if kind == "lib" else "main"]
    assert text.count(anchor) == 1, f"{anchor!r} is not once in {file}"
    if kind == "newfn":
        return text.replace(anchor, anchor + added(r, indent)) + added_fn(r)
    return text.replace(anchor, anchor + stmt(r, indent))


def base_env(home, store, target):
    env = dict(os.environ, RUSTUP_TOOLCHAIN=TOOLCHAIN, BLAZE_HOME=home, CARGO_TERM_COLOR="never",
               CARGO_NET_OFFLINE="true", CARGO_TARGET_DIR=target, BLAZE_IDLE_SECS="0")
    env["PATH"] = os.path.dirname(BLAZE) + os.pathsep + env["PATH"]
    for name in ("RUSTFLAGS", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_TARGET", "CARGO_INCREMENTAL", "BLAZE_CACHE"):
        env.pop(name, None)
    if store:
        env["BLAZE_CACHE"] = store
    return env


async def run(argv, cwd, env, step, timeout=STEP_SECS, check=True):
    """Runs `argv`; the result is (seconds, exit status, stdout, stderr). A failure names the
    step, the command, its exit status and the end of its stderr."""
    began = time.monotonic()
    proc = await asyncio.create_subprocess_exec(*argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                                stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        async with asyncio.timeout(timeout):
            out, err = await proc.communicate()
    except TimeoutError:
        proc.kill()
        await proc.wait()
        raise RuntimeError(f"{step}: {' '.join(argv)} timed out after {timeout}s")
    took = time.monotonic() - began
    out, err = out.decode(errors="replace"), err.decode(errors="replace")
    if check and proc.returncode:
        raise RuntimeError(f"{step}: {' '.join(argv)} exited {proc.returncode}: {err[-1500:]}")
    return took, proc.returncode, out, err


def how_built(stderr):
    """Whether the daemon built it, or why cargo did (as `blaze_cli.py` reads it)."""
    reason = re.search(r"^blaze: (?:bug: )?(.*?); (building with cargo|this is `cargo \w+`)(?:, which .*)?$", stderr, re.M)
    if reason:
        return "cargo: " + reason[1]
    kept = re.search(r"^blaze: (.*); nothing has changed since cargo built$", stderr, re.M)
    if kept:
        return "unchanged"
    return "daemon" if "[blaze]" in stderr else "cargo"


def checking(root):
    """Whether the `cargo check` that `blaze` starts after cargo built runs below root."""
    for pid in filter(str.isdigit, os.listdir("/proc")):
        try:
            words = open(f"/proc/{pid}/cmdline", "rb").read().split(b"\0")
            if b"check" in words[1:2] and os.readlink(f"/proc/{pid}/cwd").startswith(root):
                return True
        except OSError:
            pass
    return False


async def wait_ready(ws, env, began, step):
    """Seconds from `began` until the daemon says `ws` is ready; then waits for the
    background `cargo check` too, so that it does not run under the next timing."""
    async with asyncio.timeout(STEP_SECS):
        while True:
            _, _, out, _ = await run(["blaze", "status"], ws, env, step, check=False)
            states = re.findall(rf"^    {re.escape(ws)}: (.*)$", out, re.M)
            if states and not any(s.startswith("loading") for s in states):
                ready = time.monotonic() - began
                if not all(s == "ready" for s in states):
                    raise RuntimeError(f"{step}: daemon did not load {ws}: {states}")
                break
            if re.search(r"^daemon: not running$", out, re.M) and time.monotonic() - began > 30:
                raise RuntimeError(f"{step}: no daemon: {out[-800:]}")
            await asyncio.sleep(0.1)
        while checking(ws):
            await asyncio.sleep(0.25)
    return ready


async def traced(argv, cwd, env, step, trace):
    """Runs a `blaze build` with BLAZE_PROFILE in the directory `trace`; the result is
    run()'s and where its time went, from the profile's trace: the client's breakdown (ms,
    adding up to its wall time) and the time outside the client (cargo before and after it,
    process start)."""
    took, rc, out, err = await run(argv, cwd, dict(env, BLAZE_PROFILE=trace), step)
    try:
        ours = json.load(open(os.path.join(trace, "trace.json")))["blaze"]
        shutil.rmtree(trace)
    except (OSError, ValueError, KeyError):
        return took, rc, out, err, None
    rows = [[name, round(us / 1000, 1)] for name, us in ours["breakdown"]]
    rows.append(["outside the client", round(took * 1000 - ours["total_us"] / 1000, 1)])
    return took, rc, out, err, rows


def median_breakdown(times, breakdowns):
    """The breakdown of the run whose time is the median (the upper one of an even count)."""
    if not breakdowns or any(b is None for b in breakdowns):
        return None
    at = sorted(range(len(times)), key=lambda i: times[i])[len(times) // 2]
    return dict(seconds=round(times[at], 3), steps=breakdowns[at])


def summary(xs):
    return dict(median=round(statistics.median(xs), 3), min=round(min(xs), 3), max=round(max(xs), 3), n=len(xs), all=[round(x, 3) for x in xs])


def fetch(name, spec):
    source = os.path.join(SOURCES, name)
    if not os.path.isdir(source):
        os.makedirs(SOURCES, exist_ok=True)
        subprocess.run(["git", "clone", "-q", "--depth", "1", "--branch", spec["tag"], f"https://github.com/{spec['repo']}", source], check=True)
    head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=source, capture_output=True, text=True).stdout.strip()
    assert head == spec["rev"], f"{name} is at {head}, not at {spec['rev']}"
    return source


async def outputs(ws, target, spec, step):
    """What the program prints for `--version` and for the project's command line."""
    exe = os.path.join(target, "debug", spec["bin"])
    env = dict(os.environ, NO_COLOR="1")
    res = []
    for args in (["--version"], spec["run"]):
        _, rc, out, err = await run([exe, *args], ws, env, step, timeout=120, check=False)
        res.append((rc, out, err))
    return res


async def project(name, spec, work, runs, parts, pause=0.0):
    source = fetch(name, spec)
    top = os.path.realpath(os.path.join(work, name))
    ws, other = os.path.join(top, "a"), os.path.join(top, "b")
    tc, tr = os.path.join(top, "t-cargo"), os.path.join(top, "t-blaze")
    for path in (ws, other):
        if not os.path.isdir(path):
            shutil.copytree(source, path, symlinks=True)
    warm_store = os.path.join(top, "store-warm")
    row = dict(project=name, tag=spec["tag"], rev=spec["rev"], runs=runs)
    cargo_env = base_env(os.path.join(top, "home-cargo"), None, tc)
    await run(["cargo", "fetch"], ws, dict(cargo_env, CARGO_NET_OFFLINE="false"), f"{name} fetch")

    async def stop(env, cwd):
        await run(["blaze", "stop"], cwd, env, f"{name} stop", check=False)

    async def blaze_cold(label, store, i, cwd=ws, target=tr):
        home = os.path.join(top, f"home-{label}-{i}")
        shutil.rmtree(home, ignore_errors=True)
        os.makedirs(home, mode=0o700)
        shutil.rmtree(target, ignore_errors=True)
        env = base_env(home, store, target)
        began = time.monotonic()
        took, _, _, err = await run(["blaze", "build"], cwd, env, f"{name} {label} {i} build")
        ready = await wait_ready(cwd, env, began, f"{name} {label} {i} ready")
        print(f"  {label:16} run {i}: build {took:7.2f}s  ready {ready:7.2f}s  ({how_built(err)})", flush=True)
        return took, ready, env

    if "cold" in parts:
        # The warm store: one blaze build of the same commit in the other checkout.
        shutil.rmtree(warm_store, ignore_errors=True)
        _, ready_b, env_b = await blaze_cold("store-fill", warm_store, 0, cwd=other, target=os.path.join(top, "t-b"))
        await stop(env_b, other)
        cargo_t, cold_b, cold_r, warm_b, warm_r = [], [], [], [], []
        for i in range(1, runs + 1):
            shutil.rmtree(tc, ignore_errors=True)
            took, _, _, _ = await run(["cargo", "build"], ws, cargo_env, f"{name} cargo cold {i}")
            cargo_t.append(took)
            print(f"  {'cargo cold':16} run {i}: {took:7.2f}s", flush=True)
            store = os.path.join(top, f"store-empty-{i}")
            shutil.rmtree(store, ignore_errors=True)
            b, r, env = await blaze_cold("empty-store", store, i)
            cold_b.append(b); cold_r.append(r)
            await stop(env, ws)
            shutil.rmtree(store, ignore_errors=True)
            b, r, env = await blaze_cold("warm-store", warm_store, i)
            warm_b.append(b); warm_r.append(r)
            if i < runs:
                await stop(env, ws)
        row["cold"] = dict(cargo=summary(cargo_t), blaze_build=summary(cold_b), blaze_ready=summary(cold_r),
                           warm_store_build=summary(warm_b), warm_store_ready=summary(warm_r), store_fill_ready=round(ready_b, 3))
        shutil.rmtree(os.path.join(top, "t-b"), ignore_errors=True)
    # The daemon of the last warm-store run serves what follows; start one if there is none.
    env = base_env(os.path.join(top, "home-main"), warm_store, tr)
    if "cold" in parts:
        env = base_env(os.path.join(top, f"home-warm-store-{runs}"), warm_store, tr)
    else:
        os.makedirs(env["BLAZE_HOME"], mode=0o700, exist_ok=True)
        await run(["cargo", "build"], ws, cargo_env, f"{name} cargo setup")
        began = time.monotonic()
        await run(["blaze", "build"], ws, env, f"{name} blaze setup")
        await wait_ready(ws, env, began, f"{name} ready")
    await run(["cargo", "build"], ws, cargo_env, f"{name} cargo settle")
    await asyncio.sleep(pause)
    # The linker each side's program was linked with.
    row["linker"] = {side: subprocess.run(["readelf", "-p", ".comment", os.path.join(t, "debug", spec["bin"])], capture_output=True, text=True).stdout
                     for side, t in (("cargo", tc), ("blaze", tr))}

    trace = os.path.join(top, "profile")
    if "noop" in parts:
        c, r, built, spent = [], [], [], []
        for i in range(1, runs + 1):
            took, _, _, err, steps = await traced(["blaze", "build"], ws, env, f"{name} noop blaze {i}", trace)
            r.append(took); built.append(how_built(err)); spent.append(steps)
            took, _, _, _ = await run(["cargo", "build"], ws, cargo_env, f"{name} noop cargo {i}")
            c.append(took)
            print(f"  {'no-op':16} run {i}: blaze {r[-1]:6.2f}s ({built[-1]})  cargo {took:6.2f}s", flush=True)
        row["noop"] = dict(cargo=summary(c), blaze=summary(r), built=built, breakdown=median_breakdown(r, spent))

    if "edits" in parts:
        row["edits"] = {}
        for kind in ("main", "lib", "newfn"):
            file = os.path.join(ws, spec["lib" if kind == "lib" else "main"][0])
            pristine = open(file).read()
            c, r, built, same, spent = [], [], [], [], []
            try:
                for i in range(1, runs + 1):
                    open(file, "w").write(edit_text(kind, spec, pristine, i))
                    took, _, _, err, steps = await traced(["blaze", "build"], ws, env, f"{name} {kind} blaze {i}", trace)
                    r.append(took); built.append(how_built(err)); spent.append(steps)
                    took, _, _, _ = await run(["cargo", "build"], ws, cargo_env, f"{name} {kind} cargo {i}")
                    c.append(took)
                    a, b = await outputs(ws, tr, spec, f"{name} {kind} run"), await outputs(ws, tc, spec, f"{name} {kind} run")
                    same.append(a == b)
                    print(f"  {'edit ' + kind:16} run {i}: blaze {r[-1]:6.2f}s ({built[-1]})  cargo {took:6.2f}s  same={a == b}", flush=True)
                    if a != b:
                        print(f"    blaze: {a}\n    cargo:  {b}", flush=True)
            finally:
                open(file, "w").write(pristine)
            await run(["blaze", "build"], ws, env, f"{name} {kind} restore")
            await run(["cargo", "build"], ws, cargo_env, f"{name} {kind} restore")
            row["edits"][kind] = dict(cargo=summary(c), blaze=summary(r), built=built, same=same,
                                      breakdown=median_breakdown(r, spent))
        row["sample_output"] = (await outputs(ws, tr, spec, f"{name} sample"))[0][1].strip()
    await stop(env, ws)
    return row


def table(rows):
    """The markdown table of docs/results.md, from WORK/realbench.json."""
    def cell(x):
        return f"{x['median']:.2f} ({x['min']:.2f}-{x['max']:.2f})"
    def ratio(c, r):
        return f"{c['median'] / r['median']:.1f}x"
    lines = ["| Project | Step | cargo | blaze | cargo / blaze | blaze built by |", "|---|---|---:|---:|---:|---|"]
    for name, row in rows.items():
        label = f"{name} {row.get('tag', '')}".strip()
        if "failed" in row:
            lines.append(f"| {label} | failed | | | | {row['failed'][:200]} |")
            continue
        cold = row.get("cold")
        if cold:
            lines.append(f"| {label} | cold, empty store: command returns | {cell(cold['cargo'])} | {cell(cold['blaze_build'])} | {ratio(cold['cargo'], cold['blaze_build'])} | cargo (wrapper) |")
            lines.append(f"| | cold, empty store: daemon ready | | {cell(cold['blaze_ready'])} | {ratio(cold['cargo'], cold['blaze_ready'])} | |")
            lines.append(f"| | cold, warm store: command returns | | {cell(cold['warm_store_build'])} | {ratio(cold['cargo'], cold['warm_store_build'])} | cargo (wrapper) |")
            lines.append(f"| | cold, warm store: daemon ready | | {cell(cold['warm_store_ready'])} | {ratio(cold['cargo'], cold['warm_store_ready'])} | |")
        if "noop" in row:
            n = row["noop"]
            lines.append(f"| | no-op | {cell(n['cargo'])} | {cell(n['blaze'])} | {ratio(n['cargo'], n['blaze'])} | {', '.join(sorted(set(n['built'])))} |")
        for kind, what in (("main", "edit: body of `main`"), ("lib", "edit: body in a library"), ("newfn", "edit: new function")):
            e = row.get("edits", {}).get(kind)
            if e:
                same = f"; output same {sum(e['same'])}/{len(e['same'])}"
                lines.append(f"| | {what} | {cell(e['cargo'])} | {cell(e['blaze'])} | {ratio(e['cargo'], e['blaze'])} | {', '.join(sorted(set(e['built'])))}{same} |")
    return "\n".join(lines)


def breakdown_table(rows, top=7):
    """Where the time of each warm row's median run went: its biggest steps, then what no step
    covers and what is outside the client, in ms. The steps add up to the run's time."""
    lines = ["| Project | Step | blaze (median run) | Where the time went (ms) |", "|---|---|---:|---|"]
    for name, row in rows.items():
        label = f"{name} {row.get('tag', '')}".strip()
        parts = [("no-op", row.get("noop"))] + [(f"edit: {kind}", row.get("edits", {}).get(kind)) for kind in ("main", "lib", "newfn")]
        for what, part in parts:
            b = (part or {}).get("breakdown")
            if not b:
                continue
            fixed = ("unaccounted", "outside the client")
            steps = sorted((s for s in b["steps"] if s[0] not in fixed), key=lambda s: -s[1])
            shown = [f"{n} {ms:.1f}" for n, ms in steps[:top]]
            rest = sum(ms for _, ms in steps[top:])
            if rest:
                shown.append(f"{len(steps) - top} more {rest:.1f}")
            shown += [f"{n} {ms:.1f}" for n, ms in b["steps"] if n in fixed]
            lines.append(f"| {label} | {what} | {b['seconds']:.3f} | {', '.join(shown)} |")
            label = ""
    return "\n".join(lines)


async def main():
    args = sys.argv[1:]
    if args[:1] == ["--table"]:
        rows = json.load(open(args[1]))
        print(table(rows) + "\n\n" + breakdown_table(rows))
        return
    def opt(flag, default):
        if flag in args:
            i = args.index(flag)
            value = args[i + 1]
            del args[i:i + 2]
            return value
        return default
    runs = int(opt("--runs", "5"))
    work = os.path.abspath(opt("--work", os.path.expanduser("~/realbench")))
    parts = opt("--parts", "cold,noop,edits").split(",")
    pause = float(opt("--pause", "0"))
    names = args or list(PROJECTS)
    os.makedirs(work, exist_ok=True)
    out = os.path.join(work, "realbench.json")
    rows = json.load(open(out)) if os.path.exists(out) else {}
    host = dict(cpus=os.cpu_count(), cpu=next((l.split(":", 1)[1].strip() for l in open("/proc/cpuinfo") if l.startswith("model name")), "?"),
                load=open("/proc/loadavg").read().split()[:3], kernel=os.uname().release,
                rustc=subprocess.run(["rustc", f"+{TOOLCHAIN}", "-V"], capture_output=True, text=True).stdout.strip(),
                blaze=subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT, capture_output=True, text=True).stdout.strip())
    print(json.dumps(host), flush=True)
    for name in names:
        print(f"== {name}", flush=True)
        load = open("/proc/loadavg").read().split()[:3]
        try:
            row = await project(name, PROJECTS[name], work, runs, parts, pause)
        except Exception as e:  # A project blaze cannot build is a result.
            row = dict(project=name, failed=f"{type(e).__name__}: {e}")
            print(f"  FAILED: {row['failed']}", flush=True)
        row["host"] = host
        row["load"] = dict(before=load, after=open("/proc/loadavg").read().split()[:3])
        rows[name] = row
        json.dump(rows, open(out, "w"), indent=1)
    print(json.dumps(rows, indent=1))


if __name__ == "__main__":
    asyncio.run(main())
