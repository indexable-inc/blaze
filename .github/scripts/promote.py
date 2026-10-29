#!/usr/bin/env python3
"""promote.py: a canary release becomes stable once it passed the heavy tests (docs/release.md).

    promote.py which '{"tag": ""}'
        the release promote.yaml tests: `tag`, or the newest dated release when it is empty;
        step outputs `tag`, `sha`, `prerelease`
    promote.py tested '{"tag": "v0.YYYYMMDD.N", "sha": "<its commit>", "wait_minutes": 300}'
        main's merge gate passed on the tag's commit: blaze.yaml's run on the push of that
        commit to main ended green, the whole run, with its unit tests (`<platform> / tests`)
        run and passed on Linux x86-64, Linux AArch64 and macOS. A canary is cut once that run's Linux x86-64 build passes, before
        its tests; stable needs them. A run still going is waited for, at most `wait_minutes`;
        no run, a red one, or one still going after that is an error naming the commit and the
        run, and the release stays a canary
    promote.py promote '{"tag": "v0.YYYYMMDD.N", "run": "<run URL>", "bench": "<dir>"}'
        mark the release stable (not a prerelease) and Latest, unless a newer release is stable
        already; append the line saying so to its notes; the bench.py results under `bench`
        (bench-<os>-<project>.json) become assets of it, what the next canary is compared with
    promote.py issue '{"tag": ..., "run": ..., "results": "<dir>", "needs": {<job>: {"result": ...}}}'
        open an issue for a canary that failed, or comment on the open one of that release: the
        jobs of `needs` (the workflow's) that failed, and each failed case of the smoke.py
        results files under `results`, with the command that repeats it
    promote.py speed '{"tag": ..., "project": "zed", "os": ..., "bench": "<bench.py result>",
                       "baseline": "<the stable's, or empty>", "threshold": 25, "out": "<file>"}'
        the gate's speed check of an application: every phase of bench.py's result (cold,
        no-op, each edit, tests) is faster with blaze than with cargo, nothing differs from
        cargo's, and nothing is more than `threshold` percent worse than the stable release's
        result (bench.py --compare); `out` is the verdict in smoke.py's results form
    promote.py pointer '{"channel": "canary", "tag": "v0.YYYYMMDD.N"}'
        the channel's pointer names the release (release.yaml, for canary)

Each channel has a pointer: the asset `stable` or `canary` of the release `channels`, which holds
the channel's version and a newline. A workspace pinned to a channel (`[workspace.metadata.blaze]
version = "stable"`) reads it with one download. Made with the release `channels` the first time,
from what the releases are then.

The JSON is inline, a file's path, or `-` for stdin. The same tag is flipped: nothing is built
again and no archive changes, so a version names the same bytes before and after. Needs gh (GH_TOKEN).
Python 3.11+, stdlib.
"""
import datetime, glob, json, os, re, subprocess, sys, tempfile, time, typing
from dataclasses import dataclass, fields

REPO = os.environ.get("GITHUB_REPOSITORY", "indexable-inc/blaze")
ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
DATED = re.compile(r"v(\d+)\.(\d+)\.(\d+)")


@dataclass(frozen=True)
class Which:
    tag: str = ""


@dataclass(frozen=True)
class Tested:
    tag: str
    sha: str
    wait_minutes: int = 300


@dataclass(frozen=True)
class Promote:
    tag: str
    run: str
    bench: str = ""


@dataclass(frozen=True)
class Issue:
    tag: str
    run: str
    results: str
    needs: dict


@dataclass(frozen=True)
class Pointer:
    channel: typing.Literal["stable", "canary"]
    tag: str


@dataclass(frozen=True)
class Speed:
    tag: str
    project: str
    os: str
    bench: str
    out: str
    baseline: str = ""
    threshold: int = 25


POINTERS = "channels"
# The merge gate, whose run on the push of a canary's commit to main stable needs green.
GATE = ".github/workflows/blaze.yaml"
# Its jobs that must have run and passed on that commit: a run that skipped them (a change to
# nothing they read) is green without testing the commit.
GATE_TESTS = ("linux x64 / tests", "linux arm64 / tests", "macos arm64 / tests")
POLL_SECS = 120
# bench.py counts no difference under 50 ms.
FLOOR_SECS = 0.05


def typed(cls, text):
    """`cls` from the JSON `text` (inline, a path, or `-`); exits naming the field that is wrong."""
    if text == "-":
        text = sys.stdin.read()
    elif text and not text.lstrip().startswith("{"):
        text = open(text).read()
    try:
        value = json.loads(text or "{}")
    except json.JSONDecodeError as e:
        sys.exit(f"promote.py: the input is not JSON: {e}")
    if not isinstance(value, dict):
        sys.exit("promote.py: the input is a JSON object")
    known = {f.name: f for f in fields(cls)}
    if unknown := sorted(set(value) - set(known)):
        sys.exit(f"promote.py: no field {', '.join(unknown)} (fields: {', '.join(known)})")
    hints = typing.get_type_hints(cls)
    for name, given in value.items():
        hint = hints[name]
        if typing.get_origin(hint) is typing.Literal:
            if given not in typing.get_args(hint):
                sys.exit(f"promote.py: field {name} is one of {', '.join(map(repr, typing.get_args(hint)))}, not {given!r}")
        elif not isinstance(given, hint):
            sys.exit(f"promote.py: field {name} is a {hint.__name__}, not {given!r}")
    try:
        return cls(**value)
    except TypeError as e:
        sys.exit(f"promote.py: {e}")


def gh(*args, input=None):
    done = subprocess.run(["gh", *args], capture_output=True, text=True, input=input)
    if done.returncode:
        sys.exit(f"promote.py: `gh {' '.join(args)}` exited {done.returncode}: {done.stderr.strip()}")
    return done.stdout


def key(tag):
    return tuple(int(n) for n in DATED.fullmatch(tag).groups())


def releases():
    """Every dated release, as {tag: {isPrerelease, isLatest, ...}}."""
    found = json.loads(gh("release", "list", "-R", REPO, "--limit", "200", "--json", "tagName,isPrerelease,isLatest,isDraft"))
    return {r["tagName"]: r for r in found if DATED.fullmatch(r["tagName"]) and not r["isDraft"]}


def output(**values):
    lines = "".join(f"{k}={v}\n" for k, v in values.items())
    print(lines, end="")
    if path := os.environ.get("GITHUB_OUTPUT"):
        with open(path, "a") as f:
            f.write(lines)


def which(tag):
    known = releases()
    if not known:
        sys.exit("promote.py: no dated release yet")
    tag = tag or max(known, key=key)
    if tag not in known:
        sys.exit(f"promote.py: no release {tag} (dated releases: {', '.join(sorted(known, key=key)[-5:])})")
    sha = gh("api", f"repos/{REPO}/commits/{tag}", "-q", ".sha").strip()
    output(tag=tag, sha=sha, prerelease=str(known[tag]["isPrerelease"]).lower())


def tested(it):
    """Exits unless main's merge gate on `it.sha` is green, after waiting for it to end."""
    if not re.fullmatch(r"[0-9a-f]{40}", it.sha):
        sys.exit(f"promote.py: field sha is a commit's full hash, not {it.sha!r}")
    found = json.loads(gh("api", f"repos/{REPO}/actions/runs?head_sha={it.sha}&event=push&branch=main&per_page=20"))
    runs = [r for r in found["workflow_runs"] if r["path"] == GATE]
    if not runs:
        sys.exit(f"promote.py: {it.tag} ({it.sha[:10]}) has no run of main's merge gate ({GATE} on a push to main), "
                 "so its unit tests never ran on every platform; it stays a canary")
    # A re-run keeps its run's id and status: the newest run of the newest attempt is the verdict.
    gate = max(runs, key=lambda r: r["id"])
    deadline = time.monotonic() + it.wait_minutes * 60
    while gate["status"] != "completed":
        if time.monotonic() >= deadline:
            sys.exit(f"promote.py: main's merge gate on {it.sha[:10]} ({it.tag}) is still {gate['status']} after "
                     f"{it.wait_minutes} min: {gate['html_url']}; it stays a canary until a run of promote.yaml after it is green")
        print(f"main's merge gate on {it.sha[:10]} is {gate['status']}: {gate['html_url']}", flush=True)
        time.sleep(POLL_SECS)
        gate = json.loads(gh("api", f"repos/{REPO}/actions/runs/{gate['id']}"))
    line = f"main's merge gate on {it.sha[:10]} ({it.tag}): {gate['conclusion']}, {gate['html_url']} (attempt {gate['run_attempt']})"
    if path := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(path, "a") as f:
            f.write(f"- {line}\n")
    if gate["conclusion"] != "success":
        sys.exit(f"promote.py: {line}. Stable needs it green: re-run its failed jobs if they flaked, then "
                 f"`gh workflow run promote.yaml -f tag={it.tag}`; {it.tag} stays a canary")
    jobs = {j["name"]: j for j in json.loads(gh("api", f"repos/{REPO}/actions/runs/{gate['id']}/jobs?filter=latest&per_page=100"))["jobs"]}
    if untested := [name for name in GATE_TESTS if jobs.get(name, {}).get("conclusion") != "success"]:
        said = ", ".join(f"`{name}` {jobs[name]['conclusion'] if name in jobs else 'did not run'}" for name in untested)
        sys.exit(f"promote.py: {line}, but not every platform's unit tests passed on that commit ({said}); "
                 f"{it.tag} stays a canary")
    print(f"{line}; {', '.join(GATE_TESTS)} passed")


def promote(tag, run_url, bench):
    known = releases()
    if not known.get(tag, {}).get("isPrerelease"):
        print(f"{tag} is stable already: nothing to do")
        return
    stable = [t for t, r in known.items() if not r["isPrerelease"]]
    newest_stable = max(stable, key=key) if stable else None
    # Stable never moves backwards: an older canary that passes late is stable, not Latest.
    latest = newest_stable is None or key(tag) > key(newest_stable)
    today = datetime.datetime.now(datetime.UTC).date().isoformat()
    body = json.loads(gh("release", "view", tag, "-R", REPO, "--json", "body"))["body"].rstrip()
    line = f"Promoted to stable on {today} after {run_url}."
    if not latest:
        line += f" {newest_stable} was stable already, so it stays Latest."
    gh("release", "edit", tag, "-R", REPO, "--prerelease=false", f"--latest={'true' if latest else 'false'}",
       "--notes-file", "-", input=f"{body}\n\n---\n\n{line}\n")
    if numbers := sorted(glob.glob(os.path.join(bench, "**", "bench-*.json"), recursive=True)) if bench else []:
        gh("release", "upload", tag, "-R", REPO, "--clobber", *numbers)
    if latest:
        pointer("stable", tag)
    print(line)


def pointer(channel, tag):
    """The pointer of `channel` names `tag`; the release `channels` is made first when there is none."""
    known = releases()
    if tag not in known:
        sys.exit(f"promote.py: no release {tag} for the {channel} pointer")
    made = subprocess.run(["gh", "release", "view", POINTERS, "-R", REPO], capture_output=True).returncode == 0
    files = {channel: tag}
    if not made:
        stable = [t for t, r in known.items() if not r["isPrerelease"]]
        files = {"stable": max(stable, key=key) if stable else tag, "canary": max(known, key=key), **files}
        gh("release", "create", POINTERS, "-R", REPO, "--prerelease", "--latest=false", "--target", "main",
           "--title", "blaze's channels",
           "--notes", "Which release each channel is (docs/release.md): the assets `stable` and `canary` hold "
                      "its version. Updated by release.yaml and promote.yaml; a pin to a channel reads them.")
    with tempfile.TemporaryDirectory() as dir:
        paths = []
        for name, value in files.items():
            path = os.path.join(dir, name)
            with open(path, "w") as f:
                f.write(value.lstrip("v") + "\n")
            paths.append(path)
        gh("release", "upload", POINTERS, "-R", REPO, "--clobber", *paths)
    print(", ".join(f"{name} -> {value}" for name, value in files.items()))


def issue(tag, run_url, results, needs):
    failed_jobs = [name for name, need in needs.items() if need.get("result") not in ("success", "skipped")]
    lines = [f"Canary [{tag}](https://github.com/{REPO}/releases/tag/{tag}) failed the gate to stable, "
             f"so it stays a prerelease ([run]({run_url})). docs/release.md says what the gate is.", ""]
    if failed_jobs:
        lines += ["Jobs that did not pass: " + ", ".join(f"`{j}`" for j in failed_jobs), ""]
    for path in sorted(glob.glob(os.path.join(results, "**", "*.json"), recursive=True)):
        data = json.load(open(path))
        # bench.py's results sit beside smoke.py's: only these list their cases.
        if not isinstance(data.get("results"), list):
            continue
        platform = os.path.splitext(os.path.relpath(path, results))[0]
        bad = [r for r in data["results"] if not r["ok"] and not r["open"]]
        if not bad:
            continue
        lines += [f"### {platform} ({data['blaze']})", ""]
        for r in bad:
            lines += [f"**{r['what']}**: {r['why']}", "", f"Install it and repeat:", "", "```sh",
                      f"curl -fsSL https://ix.dev/blaze | sh -s -- --version {tag.lstrip('v')}", r["repro"], "```", ""]
            if r["stderr"]:
                lines += ["<details><summary>stderr</summary>", "", "```", r["stderr"][-4000:], "```", "", "</details>", ""]
    body = "\n".join(lines)[:60000]
    title = f"Canary {tag} is not promoted to stable"
    found = json.loads(gh("issue", "list", "-R", REPO, "--state", "open", "--search", f"in:title \"{title}\"",
                          "--json", "number,title"))
    same = [i for i in found if i["title"] == title]
    if same:
        gh("issue", "comment", str(same[0]["number"]), "-R", REPO, "--body-file", "-", input=body)
        print(f"commented on #{same[0]['number']}")
    else:
        print(gh("issue", "create", "-R", REPO, "--title", title, "--label", "bug", "--body-file", "-", input=body).strip())


def speed(it):
    """The speed check of `it.project` from bench.py's result, written to `it.out` as smoke.py
    writes its results; exits 1 when it failed."""
    data = json.load(open(it.bench))
    repro = (f"BLAZE_BIN=\"$(dirname \"$(command -v blaze)\")/blaze-engine\" "
             f"python3 harness/bench.py {it.project} --backend cranelift --runs 1 --cold-runs 1 --tests")
    checks = []

    def check(what, why, seconds=0):
        checks.append(dict(what=f"{it.project} {what}", ok=why is None, open=None, seconds=seconds, why=why,
                           repro=repro, stdout="", stderr=""))

    for key, measured in data["results"].items():
        if measured.get("failed"):
            check("bench", f"{key}: {measured['failed'][-3000:]}")
        for line in measured.get("wrong", []):
            check("bench", f"{key}: blaze's build does otherwise than cargo's: {line}")
        for label, phase in measured["phases"].items():
            pairs = [("", phase.get("wall_s"), phase.get("cargo_s"))]
            if "edit_s" in phase:
                pairs.append((" after an edit", phase["edit_s"], phase.get("edit_cargo_s")))
            for said, ours, theirs in pairs:
                if ours is None or theirs is None:
                    continue
                slower = ours > theirs and ours - theirs > FLOOR_SECS
                check(f"{label}{said}: blaze {ours:.2f} s, cargo {theirs:.2f} s",
                      f"blaze is slower than cargo ({ours:.2f} s against {theirs:.2f} s)" if slower else None, ours)
    if not data["results"]:
        check("bench", "bench.py measured nothing")
    if it.baseline and os.path.exists(it.baseline):
        compared = subprocess.run([sys.executable, os.path.join(ROOT, "harness", "bench.py"), "--compare", it.baseline,
                                   it.bench, "--threshold", str(it.threshold)], capture_output=True, text=True)
        worse = [line for line in compared.stdout.splitlines() if line.strip()] if compared.returncode else []
        if compared.returncode and not worse:
            worse = [compared.stderr.strip()[-3000:] or f"bench.py --compare exited {compared.returncode}"]
        stable = json.load(open(it.baseline)).get("commit") or "the stable release"
        check(f"against stable ({it.threshold}%)", "; ".join(f"worse than stable ({stable}): {w}" for w in worse) or None)
    else:
        print(f"{it.project}: no result of the stable release to compare with (its first gate with this check)")
    with open(it.out, "w") as f:
        json.dump(dict(blaze=f"{it.tag} speed, {it.os}", results=checks), f, indent=1)
    failed = [c for c in checks if not c["ok"]]
    lines = [f"### {it.project} speed ({it.os})", "", "| | what |", "|---|---|"]
    lines += [f"| {'ok' if c['ok'] else '**FAILED**'} | {c['what']} |" for c in checks]
    lines += [f"\n**{c['what']}**: {c['why']}" for c in failed]
    print("\n".join(lines))
    if path := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(path, "a") as f:
            f.write("\n".join(lines) + "\n")
    sys.exit(1 if failed else 0)


def main():
    args = sys.argv[1:]
    step, given = (args + [""])[:2] if len(args) <= 2 else ("", "")
    if step == "which":
        which(typed(Which, given).tag)
    elif step == "tested":
        tested(typed(Tested, given))
    elif step == "promote":
        it = typed(Promote, given)
        promote(it.tag, it.run, it.bench)
    elif step == "issue":
        it = typed(Issue, given)
        issue(it.tag, it.run, it.results, it.needs)
    elif step == "speed":
        speed(typed(Speed, given))
    elif step == "pointer":
        it = typed(Pointer, given)
        pointer(it.channel, it.tag)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
