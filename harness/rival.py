#!/usr/bin/env python3
"""rival.py: our blaze against Astral's fork of it and plain cargo, turn by turn on one machine.

    rival.py ['<json>' | FILE | -]           measure, then write the report
    rival.py report ['<json>' | FILE | -]    write the report again from the raw results

Astral (astral-experiments/blaze) branched from our main at 6b3ed181 and tunes its fork for
uv, ruff and ty. This is the score: every project, theirs and ours, built by each arm from the
same pinned source, each arm in a checkout, home, store and target directory of its own.

Per project and arm:
  cold       the first build from nothing (no target, empty store, no daemon), plus for a blaze
             arm the wait until its daemon has the workspace loaded (`ready_s`); the standard
             library comes from a remote made once by the arm's own `std` command, as a release
             ships it
  no-op      the build again with nothing changed
  body       a statement added to one function's body (bench.py's `body` edit)
  signature  a method gets a parameter and every caller the argument (bench.py's `signature`)
  tests      the project's tests (`test` of its spec), twice; the second is timed; every test
             has to end as it does under cargo
  output     after the cold build and the first of each edit the program runs (`run` of its
             spec) and has to say what cargo's says
Each phase but cold runs `runs` times. The arms take turns in a rotating order (round r starts
with arm r mod n), never at once, so a slow minute of the machine falls on every arm alike; an
edit is made in an arm's checkout, built, taken back and built again before the next arm's turn.
A blaze build that cargo did (a fallback) is recorded as such and waits for the daemon to load
again before the next timed build.

Input, one JSON object:
  arms      [{"name", "kind": "blaze"|"astral"|"cargo", "bin", "commit", "path"}]: `bin` is
            the directory with the arm's `blaze` (and `blaze-engine` or `ra-jit`), "" for cargo;
            `path` are directories put first on PATH for this arm (its toolchain); `env` are
            variables of this arm alone (RUSTUP_TOOLCHAIN: the nightly it is pinned to);
            `commit` names what was built, for the report
  projects  names from PROJECTS below (uv, ruff, ty) and bench.py's PROJECTS
  work      a directory for the checkouts, homes and stores (emptied per project)
  out       where the results go: raw/<project>.json, host.json, commands.txt, README.md, and
            beside each project's JSON the output of each arm's first edit build and tests, with
            our blaze's profile of that edit (`BLAZE_PROFILE`; ui.perfetto.dev opens its trace.json)
  runs      timed builds per phase (default 5)
  tests     run each project's tests (default true)
  interleave  the arms take turns within each phase (default true); false measures each blaze
            arm on its own beside cargo, all its phases, and stops its daemon before the next,
            for a machine that cannot hold two daemons with the project loaded (16 GB and ruff)
  machine   what the machine is, for host.json and the README (for example "ix arm64 16 vCPU")
The report subcommand takes {"out": DIR}.

Every command run is appended to commands.txt with its arm, directory and the variables it
was given beyond the machine's own.
"""
import json, os, platform, re, shutil, statistics, subprocess, sys, time, typing
from dataclasses import MISSING, dataclass, field, fields

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import bench, blaze_cli
from blaze_cli import Side, difference, program_output, test_results, wait_warm

LET = bench.LET
# The longest wait for a daemon to load: one that has not loaded by then is not waited for again.
blaze_cli.WARMUP_SECS = 600

# Astral's projects, at the revisions its downstream workflow pins (downstream-tests.yaml).
PROJECTS = {
    "uv": dict(
        repo="astral-sh/uv", tag="5b5a53e1", rev="5b5a53e1803d426ba04f4af8ff7d6ff747548218",
        build=["-p", "uv", "--bin", "uv"],
        # uv's own lock file of its Python tooling, read and printed with no network.
        run=["tree", "--frozen", "--offline", "--no-config"],
        edits=dict(
            body=bench.body("crates/uv/src/lib.rs",
                            "pub async fn run(cli: Cli, global_initialization: GlobalInitialization) -> Result<ExitStatus> {"),
            signature=bench.parameter("crates/uv-python-interpreter/src/interpreter.rs", "sys_executable", "&Path", "crates/**/*.rs"),
        ),
        test=["-p", "uv-pep440", "-p", "uv-pep508"],
    ),
    "ruff": dict(
        repo="astral-sh/ruff", tag="07872adb", rev="07872adbafa7c00f42f1174d8aa3c3e324033009",
        build=["-p", "ruff", "--bin", "ruff"],
        run=["check", "--no-cache", "--isolated", "--select", "ALL", "--output-format", "concise",
             "crates/ruff_linter/resources/test/fixtures/pyflakes"],
        edits=dict(
            body=bench.body("crates/ruff/src/lib.rs", "pub fn check(args: CheckCommand, global_options: GlobalConfigArgs) -> Result<ExitStatus> {"),
            signature=bench.parameter("crates/ruff_python_semantic/src/model.rs", "seen_typing", "bool", "crates/**/*.rs"),
        ),
        test=["-p", "ruff_python_parser", "-p", "ruff_python_ast"],
    ),
    "ty": dict(
        repo="astral-sh/ruff", tag="07872adb", rev="07872adbafa7c00f42f1174d8aa3c3e324033009",
        build=["-p", "ty", "--bin", "ty"],
        run=["check", "--no-progress", "--output-format", "concise", "--python-version", "3.12",
             "crates/ruff_linter/resources/test/fixtures/pyflakes"],
        edits=dict(
            body=bench.body("crates/ty/src/lib.rs", "fn run_check(args: CheckCommand) -> anyhow::Result<ExitStatus> {"),
            signature=bench.parameter("crates/ty_module_resolver/src/path.rs", "is_site_packages", "bool", "crates/**/*.rs"),
        ),
        test=["-p", "ty_module_resolver"],
    ),
}
EDITS = ("body", "signature")
KINDS = ("blaze", "astral", "cargo")


@dataclass(frozen=True)
class Arm:
    name: str
    kind: str
    bin: str
    commit: str
    path: list = field(default_factory=list)
    env: dict = field(default_factory=dict)


@dataclass(frozen=True)
class Input:
    arms: list
    projects: list
    work: str
    out: str
    runs: int = 5
    tests: bool = True
    interleave: bool = True
    machine: str = ""


@dataclass(frozen=True)
class Report:
    out: str


def fail(message):
    sys.exit(f"rival.py: {message}")


def checked(cls, value, where):
    """`cls` from the dict `value`, every field of its exact type; exits naming the field."""
    if not isinstance(value, dict):
        fail(f"{where} is a JSON object, not {value!r}")
    known = {f.name for f in fields(cls)}
    if unknown := sorted(set(value) - known):
        fail(f"{where} has no field {', '.join(unknown)} (fields: {', '.join(sorted(known))})")
    hints = typing.get_type_hints(cls)
    for f in fields(cls):
        if f.name not in value:
            if f.default is MISSING and f.default_factory is MISSING:
                fail(f"{where} needs field {f.name}")
            continue
        if type(value[f.name]) is not hints[f.name]:
            fail(f"{where}.{f.name} is a {hints[f.name].__name__}, not {value[f.name]!r}")
    return cls(**value)


def read(text):
    if text == "-":
        text = sys.stdin.read()
    elif text and not text.lstrip().startswith("{"):
        text = open(text).read()
    try:
        return json.loads(text or "{}")
    except json.JSONDecodeError as e:
        fail(f"the input is not JSON: {e}")


def typed(text):
    given = checked(Input, read(text), "input")
    arms = [checked(Arm, arm, f"input.arms[{at}]") for at, arm in enumerate(given.arms)]
    for at, arm in enumerate(arms):
        if arm.kind not in KINDS:
            fail(f"input.arms[{at}].kind is one of {', '.join(KINDS)}, not {arm.kind!r}")
        if (arm.kind == "cargo") != (arm.bin == ""):
            fail(f"input.arms[{at}].bin is \"\" for cargo and a directory for a blaze, not {arm.bin!r}")
        if arm.bin and not os.path.isfile(os.path.join(arm.bin, "blaze")):
            fail(f"input.arms[{at}].bin has no `blaze`: {arm.bin}")
        if not all(type(p) is str for p in arm.path):
            fail(f"input.arms[{at}].path is a list of directories, not {arm.path!r}")
        if not all(type(v) is str for v in arm.env.values()):
            fail(f"input.arms[{at}].env maps names to strings, not {arm.env!r}")
    if len({arm.name for arm in arms}) != len(arms):
        fail("input.arms has two arms of one name")
    if sum(arm.kind == "cargo" for arm in arms) != 1:
        fail("input.arms has exactly one cargo arm: every output is compared with its")
    for at, name in enumerate(given.projects):
        if type(name) is not str or name not in spec_of_all():
            fail(f"input.projects[{at}] is one of {', '.join(sorted(spec_of_all()))}, not {name!r}")
    if given.runs < 1:
        fail(f"input.runs is at least 1, not {given.runs}")
    return given, arms


def spec_of_all():
    return {**bench.PROJECTS, **PROJECTS}


class Log:
    """commands.txt: every command, its arm, where it ran and what it was given."""

    def __init__(self, path):
        self.path = path

    def ran(self, arm, ws, env, argv):
        extra = {k: v for k, v in env.items() if os.environ.get(k) != v and k != "PATH"}
        with open(self.path, "a") as out:
            out.write(f"[{arm}] cd {ws} && {' '.join(f'{k}={v}' for k, v in sorted(extra.items()))} {' '.join(argv)}\n")


class Runner:
    """One arm on one project: its checkout, its environment, how to build and what it said."""

    def __init__(self, arm, ws, work, project, spec, log):
        self.arm, self.ws, self.log = arm, ws, log
        self.home = os.path.join(work, f"home-{arm.name}")
        env = {k: v for k, v in os.environ.items() if not k.startswith(("BLAZE_", "RA_JIT_"))}
        for name in ("RUSTFLAGS", "CARGO_TARGET_DIR", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_TARGET", "RUSTC_WRAPPER", "RUSTUP_TOOLCHAIN"):
            env.pop(name, None)
        env.update(CARGO_TERM_COLOR="never", RUST_BACKTRACE="0", INSTA_UPDATE="no",
                   CARGO_UNSTABLE_BUILD_DIR_NEW_LAYOUT="false", **spec.get("env", {}), **arm.env)
        env["PATH"] = os.pathsep.join([*([arm.bin] if arm.bin else []), *arm.path, env["PATH"]])
        prefix = {"blaze": "BLAZE_", "astral": "RA_JIT_"}.get(arm.kind)
        if prefix:
            env[prefix + "HOME"] = self.home
            env[prefix + "CACHE"] = os.path.join(work, f"store-{arm.name}")
            env[prefix + "STATS"] = "1"
        self.prefix, self.env, self.loads = prefix, env, bool(prefix)
        self.side = Side(ws, env, arm.kind != "cargo")

    def run(self, command, *args, env=None):
        self.log.ran(self.arm.name, self.ws, dict(self.side.env, **(env or {})), self.side.argv(command, *args))
        return self.side.cargo(command, *args, env=env)

    def stop(self):
        if self.prefix:
            self.side.cargo("stop")

    def built(self, ran):
        """Whether a blaze arm's daemon built it, or why cargo did; "cargo" for the cargo arm."""
        if not self.prefix:
            return "cargo"
        said = ran.stdout + ran.stderr
        reason = re.search(r"^(?:blaze|ra-jit): (?:bug: )?(.*?); (building with cargo|this build is cargo's|this is `cargo \w+`|is cargo's to build|rustc links)",
                            said, re.M)
        if reason:
            return "cargo: " + reason[1][:160]
        return "daemon" if re.search(r"\[(blaze|ra-jit)\]", said) else "unknown"

    def ready(self):
        if not self.loads:
            return 0.0, ["cargo" if not self.prefix else "never loaded"]
        took, states = wait_warm(self.side, self.ws)
        if states == ["not loaded in time"]:
            self.loads = False
        return took, states


def shipped_std(arm, runner, work):
    """A remote with the arm's standard library unit, made by its own `std` command once, as its
    release would ship it."""
    remote = os.path.join(work, f"std-{arm.name}")
    if os.path.isdir(remote):
        return remote
    engine = next(os.path.join(arm.bin, n) for n in ("blaze-engine", "ra-jit") if os.path.isfile(os.path.join(arm.bin, n)))
    env = dict(runner.env, **{runner.prefix + "HOME": os.path.join(work, f"home-std-{arm.name}")})
    sysroot = subprocess.run(["rustc", "--print", "sysroot"], capture_output=True, text=True, env=env, check=True).stdout.strip()
    runner.log.ran(arm.name, work, env, [engine, "std", remote, sysroot])
    made = subprocess.run([engine, "std", remote, sysroot], capture_output=True, text=True, env=env)
    if made.returncode != 0:
        raise RuntimeError(f"{arm.name}: `std` failed:\n{made.stderr[-3000:]}")
    return remote


def fetch(name, spec, sources):
    """The pinned checkout, fetched at its revision when it is not there. Not bench.py's
    checkout: rival's uv, ruff and ty are pinned where Astral tests its fork, which bench.py's
    are not, and their `tag` is no ref to clone."""
    source = os.path.join(sources, spec["repo"].replace("/", "-") + "-" + spec["rev"][:12])
    if not os.path.isdir(source):
        os.makedirs(source + ".part", exist_ok=True)
        for argv in (["git", "init", "-q"], ["git", "fetch", "-q", "--depth", "1", f"https://github.com/{spec['repo']}", spec["rev"]],
                     ["git", "checkout", "-q", "FETCH_HEAD"]):
            subprocess.run(argv, cwd=source + ".part", check=True)
        os.rename(source + ".part", source)
    head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=source, capture_output=True, text=True).stdout.strip()
    if head != spec["rev"]:
        raise RuntimeError(f"{name} is at {head}, not at the {spec['rev']} it is pinned to")
    return source


def turns(arms, round_):
    return arms[round_ % len(arms):] + arms[:round_ % len(arms)]


def median(values):
    return round(statistics.median(values), 3) if values else None


def measure(name, spec, arms, given, log):
    work = os.path.realpath(os.path.join(given.work, name))
    shutil.rmtree(work, ignore_errors=True)
    os.makedirs(work)
    source = fetch(name, spec, os.path.join(given.work, "sources"))
    runners = {}
    for arm in arms:
        ws = os.path.join(work, arm.name)
        shutil.copytree(source, ws, symlinks=True, ignore=shutil.ignore_patterns("target", "target-ref", ".git"))
        runners[arm.name] = Runner(arm, ws, work, name, spec, log)
    build, run = spec.get("build", []), spec.get("run")
    result = dict(project=name, repo=spec["repo"], rev=spec["rev"], arms={a.name: dict(kind=a.kind, phases={}, wrong=[], failed=None) for a in arms})
    cargo_arm = next(a for a in arms if a.kind == "cargo")
    expected = {}

    def row(arm):
        return result["arms"][arm.name]

    def live(arm):
        return row(arm)["failed"] is None

    def output_is_right(arm, label):
        if run is None or not live(arm):
            return
        _, ran = runners[arm.name].run("run", *build, "--", *run)
        said = program_output(ran)
        if arm.kind == "cargo":
            expected["out"] = said
        elif "out" in expected and (wrong := difference(said, expected["out"])):
            row(arm)["wrong"].append(f"{label}: {wrong}"[:400])

    def timed_build(arm):
        r = runners[arm.name]
        took, ran = r.run("build", *build)
        if ran.returncode != 0:
            row(arm)["failed"] = f"build failed: {(ran.stderr or ran.stdout)[-1500:]}"
            print(f"    {arm.name}: build FAILED\n" + "".join(f"      {l}\n" for l in (ran.stderr or ran.stdout).strip().splitlines()[-15:]), flush=True)
            return None
        built = r.built(ran)
        if built.startswith("cargo:") or built == "unknown":
            r.ready()
        return round(took, 3), built

    for arm in arms:
        runners[arm.name].stop()
    for arm in arms:
        r = runners[arm.name]
        if r.prefix:
            try:
                r.env[r.prefix + "STD_FROM"] = shipped_std(arm, r, given.work)
            except (RuntimeError, subprocess.CalledProcessError) as e:
                row(arm)["failed"] = str(e)[-1500:]
    if subprocess.run(["cargo", "fetch"], cwd=runners[cargo_arm.name].ws, env=runners[cargo_arm.name].env, capture_output=True).returncode != 0:
        print("  cargo fetch failed; builds fetch what they need", flush=True)

    print(f"{name}: cold", flush=True)
    for arm in turns(arms, len(name)):
        if not live(arm):
            continue
        r = runners[arm.name]
        took, ran = r.run("build", *build)
        if ran.returncode != 0:
            row(arm)["failed"] = f"cold build failed: {(ran.stderr or ran.stdout)[-1500:]}"
            print(f"  {arm.name:10} cold FAILED\n" + "".join(f"      {l}\n" for l in (ran.stderr or ran.stdout).strip().splitlines()[-15:]), flush=True)
            continue
        ready, states = r.ready()
        row(arm)["phases"]["cold"] = dict(build_s=round(took, 3), ready_s=round(ready, 3), wall_s=round(took + ready, 3),
                                          built=r.built(ran), states=states)
        print(f"  {arm.name:10} cold {took:8.2f}s + ready {ready:6.2f}s  {r.built(ran)}  {states}", flush=True)
    for arm in [cargo_arm] + [a for a in arms if a.kind != "cargo"]:
        output_is_right(arm, "cold")

    def phase(label, samples):
        for arm in arms:
            got = samples[arm.name]
            if got:
                row(arm)["phases"][label] = dict(wall_s=median([s[0] for s in got]), samples_s=[s[0] for s in got],
                                                 built=sorted({s[1] for s in got}), **({"back_s": median([s[2] for s in got])} if len(got[0]) > 2 else {}))
        print(f"  {label:10} " + "  ".join(f"{a.name} {row(a)['phases'][label]['wall_s']:.3f}s" for a in arms if label in row(a)["phases"]), flush=True)

    print(f"{name}: no-op", flush=True)
    samples = {a.name: [] for a in arms}
    for round_ in range(given.runs):
        for arm in turns(arms, round_):
            if live(arm) and (got := timed_build(arm)):
                samples[arm.name].append(got)
    phase("no-op", samples)

    for kind in EDITS:
        changes = spec["edits"].get(kind)
        if not changes:
            continue
        samples = {a.name: [] for a in arms}
        rejected = {}
        for round_ in range(given.runs):
            for arm in turns(arms, round_):
                if not live(arm) or arm.name in rejected:
                    continue
                r = runners[arm.name]
                bench.write_edit(r.ws, changes, False)
                trace = os.path.realpath(os.path.join(given.out, "raw", f"{name}-{arm.name}-{kind}.profile"))
                took, ran = r.run("build", *build, env={"BLAZE_PROFILE": trace} if round_ == 0 and r.prefix == "BLAZE_" else None)
                if round_ == 0:
                    with open(os.path.join(given.out, "raw", f"{name}-{arm.name}-{kind}.txt"), "w") as f:
                        f.write(f"took {took:.3f}s, exit {ran.returncode}\n--- stdout\n{ran.stdout[-20000:]}\n--- stderr\n{ran.stderr[-40000:]}")
                if ran.returncode != 0:
                    rejected[arm.name] = (ran.stderr or ran.stdout)[-1500:]
                    print(f"    {arm.name}: {kind} edit rejected\n" + "".join(f"      {l}\n" for l in rejected[arm.name].strip().splitlines()[-8:]), flush=True)
                    made = None
                else:
                    made = round(took, 3), r.built(ran)
                    if made[1].startswith("cargo:") or made[1] == "unknown":
                        r.ready()
                if made and round_ == 0:
                    output_is_right(arm, kind) if arm.kind != "cargo" else None
                bench.write_edit(r.ws, changes, True)
                back = timed_build(arm)
                if made and back:
                    samples[arm.name].append((made[0], made[1], back[0]))
        phase(kind, samples)
        for arm in arms:
            if arm.kind == "cargo" or not live(arm):
                continue
            if (cargo_arm.name in rejected) != (arm.name in rejected):
                said = "builds" if arm.name not in rejected else "rejects"
                row(arm)["wrong"].append(f"{kind}: {said} the edit cargo {'rejects' if said == 'builds' else 'builds'}"[:400])
            if arm.name in rejected:
                row(arm)["phases"][kind] = dict(rejected=rejected[arm.name][-400:])
        if cargo_arm.name in rejected:
            row(cargo_arm)["phases"][kind] = dict(rejected=rejected[cargo_arm.name][-400:])

    if given.tests and spec.get("test") is not None:
        args = spec["test"]
        print(f"{name}: tests", flush=True)
        outcomes = {}
        for round_ in range(2):
            for arm in turns(arms, round_):
                if not live(arm):
                    continue
                took, ran = runners[arm.name].run("test", *args)
                outcomes[arm.name] = (round(took, 3), ran)
                if round_ == 0:
                    row(arm)["phases"]["tests_first"] = dict(wall_s=round(took, 3))
        cargo_said = test_results(outcomes[cargo_arm.name][1]) if cargo_arm.name in outcomes else None
        for arm in arms:
            if arm.name not in outcomes:
                continue
            took, ran = outcomes[arm.name]
            with open(os.path.join(given.out, "raw", f"{name}-{arm.name}-tests.txt"), "w") as f:
                f.write(f"kept: {getattr(ran, 'kept', [])}\n--- stdout\n{ran.stdout}\n--- stderr\n{ran.stderr[-20000:]}")
            status, tests, totals = test_results(ran)
            entry = dict(wall_s=took, status=status, passed=totals[1], failed=totals[2], ignored=totals[3],
                         built=runners[arm.name].built(ran))
            if arm.kind != "cargo" and cargo_said and (wrong := difference((status, tests, totals), cargo_said)):
                entry["differs"] = wrong[:400]
                row(arm)["wrong"].append(f"tests: {wrong}"[:400])
            row(arm)["phases"]["tests"] = entry
            print(f"  {arm.name:10} tests {took:8.2f}s  {totals[1]} passed {totals[2]} failed  {entry.get('differs', 'same as cargo') if arm.kind != 'cargo' else ''}", flush=True)

    for arm in arms:
        runners[arm.name].stop()
    return result


def host(given, arms):
    def out(argv):
        ran = subprocess.run(argv, capture_output=True, text=True)
        return ran.stdout.strip() if ran.returncode == 0 else None
    mem = open("/proc/meminfo").read().split("\n")[0] if os.path.exists("/proc/meminfo") else None
    return dict(machine=given.machine, platform=platform.platform(), cpus=os.cpu_count(), affinity=len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else None,
                memory=mem, cargo=out(["cargo", "--version"]), date=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                arms=[dict(name=a.name, kind=a.kind, commit=a.commit) for a in arms], runs=given.runs, interleave=given.interleave)


PHASES = ("cold", "no-op", "body", "signature", "tests")


def report(out):
    """README.md from raw/*.json and host.json: one row per project and phase, each arm's median,
    and the gap of our blaze to the best other arm (above 1.00x: someone beats us)."""
    info = json.load(open(os.path.join(out, "host.json")))
    rows = [json.load(open(os.path.join(out, "raw", f))) for f in sorted(os.listdir(os.path.join(out, "raw"))) if f.endswith(".json")]
    names = [a["name"] for a in info["arms"]]
    ours = next(a["name"] for a in info["arms"] if a["kind"] == "blaze")
    lines = [f"# Rival score: our blaze, Astral's fork and cargo ({info['date'][:10]})", "",
             f"Machine: {info['machine'] or info['platform']}, {info['affinity'] or info['cpus']} cores used. "
             f"{info['runs']} timed builds per phase, "
             f"{'arms in turn' if info.get('interleave', True) else 'each blaze arm on its own beside cargo, one daemon at a time'} (harness/rival.py). Commits: "
             + ", ".join(f"{a['name']} ({a['kind']}) `{a['commit'][:10]}`" for a in info["arms"]) + ".", "",
             "Seconds, median. cold = first build + the wait until the daemon is ready. tests = the second `test` run. "
             f"gap = {ours} / fastest other arm: above 1.00x someone beats us. A cell with `!` was built by cargo "
             "(the daemon declined); `x` failed; `rejected`: the edit did not build.", "",
             "| project | phase | " + " | ".join(names) + " | gap |", "|---|---|" + "---:|" * (len(names) + 1)]
    for r in rows:
        for label in PHASES:
            cells, times = [], {}
            for n in names:
                arm = r["arms"].get(n, {})
                p = arm.get("phases", {}).get(label)
                if p is None:
                    cells.append("x" if arm.get("failed") else "")
                    continue
                if "rejected" in p:
                    cells.append("rejected")
                    continue
                times[n] = p["wall_s"]
                built = p.get("built")
                flag = "!" if (isinstance(built, list) and any(b.startswith("cargo:") for b in built)) or (isinstance(built, str) and built.startswith("cargo:")) else ""
                extra = f" ({p['passed']} ok, {p['failed']} failed)" if label == "tests" else ""
                cells.append(f"{p['wall_s']:.2f}{flag}{extra}")
            if not any(cells):
                continue
            others = [t for n, t in times.items() if n != ours]
            gap = f"{times[ours] / min(others):.2f}x" if ours in times and others and min(others) > 0 else ""
            lines.append(f"| {r['project']} | {label} | " + " | ".join(cells) + f" | {gap} |")
    wrong = [(r["project"], n, w) for r in rows for n, a in r["arms"].items() for w in a.get("wrong", [])]
    failed = [(r["project"], n, a["failed"]) for r in rows for n, a in r["arms"].items() if a.get("failed")]
    lines += ["", "## Wrong output or tests (against cargo)", ""]
    lines += [f"- {p} / {n}: {w}" for p, n, w in wrong] or ["None."]
    lines += ["", "## Failed", ""]
    lines += [f"- {p} / {n}: {f.splitlines()[0][:200] if f else ''}" for p, n, f in failed] or ["None."]
    lines += ["", "Raw results: raw/<project>.json. Every command: commands.txt. Machine: host.json.", ""]
    open(os.path.join(out, "README.md"), "w").write("\n".join(lines))
    print("\n".join(lines))


def main():
    argv = sys.argv[1:]
    if argv[:1] == ["report"]:
        if len(argv) > 2:
            fail(f"usage: rival.py report ['<json>' | FILE | -]")
        report(checked(Report, read(argv[1] if len(argv) == 2 else ""), "input").out)
        return
    if len(argv) != 1:
        fail(f"usage: rival.py ['<json>' | FILE | -]\n\n{__doc__}")
    given, arms = typed(argv[0])
    os.makedirs(os.path.join(given.out, "raw"), exist_ok=True)
    json.dump(host(given, arms), open(os.path.join(given.out, "host.json"), "w"), indent=1)
    log = Log(os.path.join(given.out, "commands.txt"))
    specs = spec_of_all()
    cargo_arm = next(a for a in arms if a.kind == "cargo")
    blocks = [arms] if given.interleave else [[cargo_arm, a] for a in arms if a.kind != "cargo"]
    for name in given.projects:
        try:
            result = None
            for block in blocks:
                got = measure(name, specs[name], block, given, log)
                if result is None:
                    result = got
                else:
                    result["arms"].update((k, v) for k, v in got["arms"].items() if k != cargo_arm.name)
        except (RuntimeError, subprocess.CalledProcessError, OSError, AssertionError) as e:
            result = dict(project=name, repo=specs[name]["repo"], rev=specs[name]["rev"], arms={}, error=str(e)[-2000:])
            print(f"{name}: {e}", flush=True)
        json.dump(result, open(os.path.join(given.out, "raw", f"{name}.json"), "w"), indent=1)
    report(given.out)


if __name__ == "__main__":
    main()
