#!/usr/bin/env python3
"""smoke.py: small programs and real projects that must build and run correctly with `blaze run`
and `blaze test`, in dev and --release (docs/release.md, "The smoke corpus").

usage: smoke.py [INPUT]          run the corpus
       smoke.py apt [INPUT]      the Debian packages the selected projects build with
       smoke.py brew [INPUT]     the Homebrew packages they build with on macOS

INPUT is one JSON object (inline, a file's path, or `-` for stdin; none is `{}`), every field
optional:

    {"groups": ["std", "crates", "projects", "apps"], "only": [NAME, ...], "profiles": ["dev", "release"],
     "blaze": "path of the `blaze` to run (default: the one on PATH)", "work": DIR,
     "results": "a JSON file of every result", "blaze_rev": "the commit of the project `blaze`"}

smoke/smoke.json names the cases and projects. A case is a directory smoke/<name>: a cargo
project whose `blaze run` prints exactly smoke/<name>/expected.txt (given `args` and `env`) and
whose `blaze test` passes `tests` tests. A project is a repository at a pinned commit (`rev`;
`{blaze}` is --blaze-rev, by default this checkout's HEAD), whose `runs` print what they say
(`stdout` exactly, or `contains`, or a `file` that `starts` so, each character of it a byte)
and whose `blaze test` (with the arguments of `test`) passes. A private repository is fetched with GH_TOKEN. A project may have
`env` for its builds, `seconds` for each command (default 3600), and `prepare`, a module of the
harness whose `prepare(checkout)` gives the tree to build and the environment its builds need
(bun: bunprep.py, its own build of its C and C++; flox: floxprep.py, its Nix build's), and
`fetches`, the selections (`-p`s, `--tests`) `blaze lock --fetches` records its build scripts'
downloads for first, one per feature set they pick it by (fframes'). Its `builds` are `blaze build` with `args` in
the checkout's `dir` (a workspace of its own, as dynamo's Python bindings are), each then running
`then` (`{target}` is that directory's target/<profile>) to print what it says. `macos` holds the
fields that are otherwise on macOS, and `excluded` says what the corpus leaves out of a project
and why (printed with its results), so that the list shrinks.

Groups: `std` (no dependencies: the merge gate's, every pull request), `crates` (from
crates.io), `projects` (a project's default), and `apps` (the large applications, each in a job
of its own in promote.yaml, with bench.py's times beside cargo's); all four are the gate a canary
passes to become stable. Without `groups`, every group but `apps` runs.

A case or project with `open` (an issue) is run and reported, and its failure does not fail the
run. Each failure is printed with the command that repeats it. `results` is every result, for
promote.yaml's issues. Stdlib only.
"""
import base64, importlib, json, os, shlex, shutil, subprocess, sys, tempfile, time, typing
from dataclasses import dataclass, fields

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
CORPUS = os.path.join(HERE, "smoke")
# A project's `prepare` is a module beside this one.
sys.path.insert(0, HERE)
EXE = ".exe" if os.name == "nt" else ""
Group = typing.Literal["std", "crates", "projects", "apps"]
Profile = typing.Literal["dev", "release"]
GROUPS = ("std", "crates", "projects")
CASE_SECS = 900
PROJECT_SECS = 3600
# What blaze prints when something of the build was not its own (install.yaml looks for these too).
NOT_BLAZES = ("blaze: bug:", "the daemon has no part")


@dataclass(frozen=True)
class Input:
    groups: tuple[Group, ...] = GROUPS
    only: tuple[str, ...] = ()
    profiles: tuple[Profile, ...] = ("dev", "release")
    blaze: str = ""
    work: str = ""
    results: str = ""
    blaze_rev: str = ""


def typed(text):
    """`Input` from INPUT; exits naming the field that is wrong."""
    if text == "-":
        text = sys.stdin.read()
    elif text and not text.lstrip().startswith("{"):
        text = open(text).read()
    try:
        value = json.loads(text or "{}")
    except json.JSONDecodeError as e:
        sys.exit(f"smoke.py: INPUT is not JSON: {e}")
    if not isinstance(value, dict):
        sys.exit("smoke.py: INPUT is a JSON object")
    hints = typing.get_type_hints(Input)
    known = {f.name for f in fields(Input)}
    if unknown := sorted(set(value) - known):
        sys.exit(f"smoke.py: INPUT has no field {', '.join(unknown)} (fields: {', '.join(sorted(known))})")
    for name, given in value.items():
        hint = hints[name]
        if hint is str:
            ok = isinstance(given, str)
        else:
            (item, _) = typing.get_args(hint)
            allowed = typing.get_args(item) if typing.get_origin(item) is typing.Literal else None
            ok = isinstance(given, list) and all(isinstance(it, str) and (allowed is None or it in allowed) for it in given)
            if ok:
                value[name] = tuple(given)
            elif allowed:
                sys.exit(f"smoke.py: INPUT field {name} is a list of {', '.join(map(repr, allowed))}, not {given!r}")
        if not ok:
            sys.exit(f"smoke.py: INPUT field {name} is a {'string' if hint is str else 'list of strings'}, not {given!r}")
    return Input(**value)


def blaze_program(given):
    found = given or shutil.which("blaze")
    if not found or not os.path.isfile(found):
        sys.exit(f"smoke.py: no blaze {found or 'on PATH'}: INPUT's `blaze` names a build's, or an installed one is on PATH")
    return os.path.abspath(found)


def base_env():
    env = dict(os.environ, CARGO_TERM_COLOR="never", RUST_BACKTRACE="0")
    for name in ("RUSTFLAGS", "CARGO_TARGET_DIR", "CARGO_BUILD_TARGET", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"):
        env.pop(name, None)
    return env


def shown(argv, env=None, cwd=None):
    """The command as a shell line a person can paste."""
    sets = [f"{k}={shlex.quote(v)}" for k, v in (env or {}).items()]
    line = " ".join(sets + [shlex.quote(a) for a in argv])
    return f"cd {shlex.quote(cwd)} && {line}" if cwd else line


def run(argv, cwd, env, timeout):
    began = time.monotonic()
    try:
        done = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, timeout=timeout)
        code, out, err = done.returncode, done.stdout.decode(errors="replace"), done.stderr.decode(errors="replace")
    except subprocess.TimeoutExpired as e:
        code, out = None, (e.stdout or b"").decode(errors="replace")
        err = (e.stderr or b"").decode(errors="replace") + f"\n(no answer after {timeout} s)"
    return code, out, err, time.monotonic() - began


def test_counts(out):
    """(passed, failed) summed over libtest's `test result:` lines."""
    passed = failed = 0
    for line in out.splitlines():
        words = line.split()
        if line.startswith("test result:") and "passed;" in words and "failed;" in words:
            passed += int(words[words.index("passed;") - 1])
            failed += int(words[words.index("failed;") - 1])
    return passed, failed


def tail(text, lines=25):
    return "\n".join(text.rstrip().splitlines()[-lines:])


def fetch(name, spec, rev, dest):
    """`dest` checked out at `rev` of `spec["repo"]`, fetched alone (no history)."""
    if os.path.isdir(os.path.join(dest, ".git")):
        head = subprocess.run(["git", "-C", dest, "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
        if head == rev:
            return
        shutil.rmtree(dest)
    os.makedirs(dest)
    auth = []
    if spec.get("private"):
        token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
        if not token:
            raise RuntimeError(f"{spec['repo']} is private: set GH_TOKEN to a token that reads it")
        basic = base64.b64encode(f"x-access-token:{token}".encode()).decode()
        auth = ["-c", f"http.https://github.com/.extraheader=AUTHORIZATION: basic {basic}"]
    for argv in (["git", "init", "-q", dest],
                 ["git", *auth, "-C", dest, "fetch", "-q", "--depth", "1", f"https://github.com/{spec['repo']}", rev],
                 ["git", "-C", dest, "checkout", "-q", "FETCH_HEAD"]):
        done = subprocess.run(argv, capture_output=True, text=True, timeout=1800)
        if done.returncode:
            raise RuntimeError(f"{name}: `{' '.join(argv[-3:])}` failed: {done.stderr.strip()}")


class Smoke:
    def __init__(self, args):
        self.blaze = blaze_program(args.blaze)
        self.work = os.path.abspath(args.work or tempfile.mkdtemp(prefix="blaze-smoke-"))
        self.profiles = args.profiles
        self.blaze_rev = args.blaze_rev
        self.results = []

    def check(self, what, argv, cwd, env, timeout, judge, repro, open_issue):
        code, out, err, secs = run(argv, cwd, env, timeout)
        not_ours = [line for line in err.splitlines() if any(it in line for it in NOT_BLAZES)]
        why = f"the build was not blaze's: {not_ours[0].strip()}" if code == 0 and not_ours else judge(code, out)
        result = dict(what=what, ok=why is None, open=open_issue, seconds=round(secs, 1), why=why,
                      repro=repro, stdout=tail(out), stderr=tail(err))
        self.results.append(result)
        if why is None:
            mark = "ok, OPEN passes now (remove its mark)" if open_issue else "ok"
        else:
            mark = f"OPEN ({open_issue})" if open_issue else "FAILED"
        print(f"{mark:<8} {what} ({secs:.1f} s)", flush=True)
        if why is not None:
            print(f"    {why}\n    repeat: {repro}", flush=True)
            for label, text in (("stdout", out), ("stderr", err)):
                if text.strip():
                    print(f"    --- {label}\n" + "\n".join("    " + line for line in tail(text).splitlines()), flush=True)

    def profile_args(self, profile):
        return ["--release"] if profile == "release" else []

    def case(self, name, spec):
        source = os.path.join(CORPUS, name)
        cwd = os.path.join(self.work, "cases", name)
        shutil.rmtree(cwd, ignore_errors=True)
        shutil.copytree(source, cwd, ignore=shutil.ignore_patterns("target"))
        expected = open(os.path.join(source, "expected.txt")).read()
        extra = spec.get("env", {})
        env = dict(base_env(), **extra)
        tests = spec["tests"]

        def ran(code, out):
            if code != 0:
                return f"exit {code}"
            if out != expected:
                return "stdout is not expected.txt:\n" + "".join(
                    f"      want {w!r}\n      got  {g!r}\n" for w, g in zip_longest(expected, out) if w != g)
            return None

        def tested(code, out):
            passed, failed = test_counts(out)
            if code != 0 or failed:
                return f"exit {code}, {passed} passed, {failed} failed"
            if passed != tests:
                return f"{passed} tests passed, {tests} expected"
            return None

        for profile in self.profiles:
            p = self.profile_args(profile)
            argv = [self.blaze, "run", *p, "--", *spec.get("args", [])]
            repro = shown(["blaze", "run", *p, "--", *spec.get("args", [])], extra, f"harness/smoke/{name}")
            self.check(f"{name} {profile} run", argv, cwd, env, CASE_SECS, ran, repro, spec.get("open"))
            argv = [self.blaze, "test", *p]
            self.check(f"{name} {profile} test", argv, cwd, env, CASE_SECS, tested,
                       shown(["blaze", "test", *p], extra, f"harness/smoke/{name}"), spec.get("open"))

    def project(self, name, spec):
        if sys.platform == "darwin":
            spec = {**spec, **spec.get("macos", {})}
        for left_out, why in spec.get("excluded", {}).items():
            print(f"excluded {name}: {left_out}: {why}", flush=True)
        rev = spec["rev"].replace("{blaze}", self.blaze_rev)
        cwd = os.path.join(self.work, "projects", name)
        out_dir = os.path.join(self.work, "out", name)
        os.makedirs(out_dir, exist_ok=True)
        clone = f"git clone https://github.com/{spec['repo']} {name} && cd {name} && git checkout {rev}"
        try:
            fetch(name, spec, rev, cwd)
        except (RuntimeError, subprocess.TimeoutExpired) as e:
            self.results.append(dict(what=f"{name} fetch", ok=False, open=spec.get("open"), seconds=0,
                                     why=str(e), repro=clone, stdout="", stderr=""))
            print(f"FAILED   {name} fetch\n    {e}", flush=True)
            return
        env = dict(base_env(), **spec.get("env", {}))
        if "prepare" in spec:
            began = time.monotonic()
            try:
                cwd, prepared = importlib.import_module(spec["prepare"]).prepare(cwd)
            except (Exception, SystemExit) as e:
                self.results.append(dict(what=f"{name} prepare", ok=False, open=spec.get("open"),
                                         seconds=round(time.monotonic() - began, 1), why=str(e)[-3000:],
                                         repro=f"{clone} && python3 harness/{spec['prepare']}.py .", stdout="", stderr=""))
                print(f"FAILED   {name} prepare\n    {str(e)[-3000:]}", flush=True)
                return
            env.update(prepared)
            print(f"ok       {name} prepare ({time.monotonic() - began:.1f} s)", flush=True)
        seconds = spec.get("seconds", PROJECT_SECS)
        places = {"{work}": out_dir, "{input}": os.path.join(out_dir, "input.txt")}
        if "input" in spec:
            with open(places["{input}"], "w") as f:
                f.write(spec["input"])

        def fill(args):
            for key, value in places.items():
                args = [a.replace(key, value) for a in args]
            return args

        def judged(step):
            def judge(code, out):
                if code != 0:
                    return f"exit {code}"
                if "stdout" in step and out != step["stdout"]:
                    return f"stdout {out!r}, want {step['stdout']!r}"
                if "contains" in step and step["contains"] not in out:
                    return f"stdout has no {step['contains']!r}"
                return None
            return judge

        for i, selection in enumerate(spec.get("fetches", [])):
            argv = ["lock", "--fetches", *selection]
            self.check(f"{name} lock {i}", [self.blaze, *argv], cwd, env, seconds, judged({}),
                       f"{clone} && {shown(['blaze', *argv], spec.get('env'))}", spec.get("open"))
        for profile in self.profiles:
            p = self.profile_args(profile)
            for i, step in enumerate(spec.get("builds", [])):
                at = os.path.join(cwd, step.get("dir", "."))
                built = shown(["blaze", "build", *p, *step.get("args", [])], spec.get("env"), step.get("dir"))
                self.check(f"{name} {profile} build {i}", [self.blaze, "build", *p, *step.get("args", [])], at, env,
                           seconds, judged({}), f"{clone} && {built}", spec.get("open"))
                if "then" in step:
                    out_of = os.path.join("target", "release" if profile == "release" else "debug")
                    then = [a.replace("{target}", os.path.join(at, out_of)) for a in fill(step["then"])]
                    self.check(f"{name} {profile} build {i} then", then, at, env, seconds, judged(step),
                               f"{clone} && {built} && {shown([a.replace('{target}', out_of) for a in step['then']])}",
                               spec.get("open"))
            for i, step in enumerate(spec.get("runs", [])):
                args = [*p, *step.get("select", []), "--", *fill(step["args"])]
                file = fill([step["file"]])[0] if "file" in step else None
                if file and os.path.exists(file):
                    os.remove(file)

                def ran(code, out, step=step, file=file):
                    if why := judged(step)(code, out):
                        return why
                    if file:
                        if not os.path.isfile(file):
                            return f"it wrote no {file}"
                        with open(file, "rb") as f:
                            if not f.read(len(step["starts"])) == step["starts"].encode("latin-1"):
                                return f"{file} does not start with {step['starts']!r}"
                    return None

                self.check(f"{name} {profile} run {i}", [self.blaze, "run", *args], cwd, env, seconds, ran,
                           f"{clone} && {shown(['blaze', 'run', *args], spec.get('env'))}", spec.get("open"))
            if "test" in spec:
                args = [*p, *spec["test"]]

                def tested(code, out):
                    passed, failed = test_counts(out)
                    if code != 0 or failed:
                        return f"exit {code}, {passed} passed, {failed} failed"
                    return None if passed else "no test ran"

                self.check(f"{name} {profile} test", [self.blaze, "test", *args], cwd, env, seconds, tested,
                           f"{clone} && {shown(['blaze', 'test', *args], spec.get('env'))}", spec.get("open"))


def zip_longest(a, b):
    """The lines of `a` and `b` side by side, '' past the end of the shorter."""
    a, b = a.splitlines(keepends=True), b.splitlines(keepends=True)
    return [(a[i] if i < len(a) else "", b[i] if i < len(b) else "") for i in range(max(len(a), len(b)))]


def summary(results, out):
    failed = [r for r in results if not r["ok"] and not r["open"]]
    lines = ["| | what | s |", "|---|---|---|"]
    for r in results:
        mark = "ok" if r["ok"] else (f"open {r['open']}" if r["open"] else "**FAILED**")
        lines.append(f"| {mark} | {r['what']} | {r['seconds']} |")
    for r in failed:
        lines += ["", f"**{r['what']}**: {r['why']}", "", "```sh", r["repro"], "```"]
    out.write("\n".join(lines) + "\n")
    return failed


def main():
    words = sys.argv[1:]
    packages = words[0] if words[:1] in (["apt"], ["brew"]) else None
    if packages:
        words = words[1:]
    if len(words) > 1 or words[:1] in (["-h"], ["--help"]):
        sys.exit(__doc__)
    args = typed(words[0] if words else "")
    corpus = json.load(open(os.path.join(CORPUS, "smoke.json")))
    named = set(corpus["cases"]) | set(corpus["projects"])
    if unknown := sorted(set(args.only) - named):
        sys.exit(f"smoke.py: INPUT field only: no case or project {', '.join(unknown)}")
    cases = {n: s for n, s in corpus["cases"].items() if s["group"] in args.groups and (not args.only or n in args.only)}
    projects = {n: s for n, s in corpus["projects"].items()
                if s.get("group", "projects") in args.groups and (not args.only or n in args.only)}
    if left := sorted(set(args.only) - set(cases) - set(projects)):
        sys.exit(f"smoke.py: INPUT field only names {', '.join(left)}, which INPUT's groups ({', '.join(args.groups)}) leave out")
    if packages:
        print(" ".join(sorted({p for s in projects.values() for p in s.get(packages, [])})))
        return
    if not args.blaze_rev:
        rev = subprocess.run(["git", "-C", ROOT, "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
        args = Input(**{**args.__dict__, "blaze_rev": rev})
    smoke = Smoke(args)
    version = (subprocess.run([smoke.blaze, "--version"], capture_output=True, text=True).stdout.strip().splitlines() or [""])[0]
    print(f"{version or smoke.blaze}: {len(cases)} cases, {len(projects)} projects, {', '.join(args.profiles)}; work {smoke.work}", flush=True)
    began = time.monotonic()
    try:
        for name, spec in cases.items():
            smoke.case(name, spec)
        for name, spec in projects.items():
            smoke.project(name, spec)
    finally:
        subprocess.run([smoke.blaze, "stop"], capture_output=True)
    print(f"{time.monotonic() - began:.0f} s", flush=True)
    if args.results:
        with open(args.results, "w") as f:
            json.dump(dict(blaze=version, results=smoke.results), f, indent=1)
    failed = summary(smoke.results, sys.stdout)
    if step_summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(step_summary, "a") as f:
            f.write(f"### smoke corpus: {version}\n\n")
            summary(smoke.results, f)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
