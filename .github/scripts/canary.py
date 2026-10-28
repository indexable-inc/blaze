#!/usr/bin/env python3
"""canary.py: a canary of every built main, cut by a tag on main's own commit (docs/release.md).

    canary.py cut '{"push": false, "min_minutes": 30, "wait": "<sha>", "wait_minutes": 90}'
        the newest commit of main (first parents, since the newest release) that main's merge
        gate built: the job `linux x64 / build` of blaze.yaml's run on the push of that commit
        passed. Not the whole run: no unit tests, no other platform (stable waits for those,
        promote.py tested). When it has changes since that release beyond the bots' own and
        the docs: the tag v0.YYYYMMDD.N on that commit, made through the API, and release.yaml
        dispatched on it.
        With `wait` (canary.yaml on a push to main passes the pushed commit), it first waits
        for the newest commit of main that has a build to finish it, at most `wait_minutes`,
        so the cut comes as soon as that build is done; a build still running then is an error
        naming it. Nothing when a release was published less than `min_minutes` ago or
        release.yaml is running: the next push, or the hourly run, cuts it then. Without
        `push`, what it would tag is shown and nothing leaves the checkout.
    canary.py notes '{"tag": "v0.YYYYMMDD.N", "notes": "<file>"}'
        release.yaml's notes job: the release notes, CHANGELOG.md's [Unreleased] lines added since
        the previous release and the title of each change merged since then, and the step output
        `version`, the tag's. Nothing in the checkout changes: the build is told the release by
        `BLAZE_RELEASE` (crates/blaze-engine/build.rs), never by a version written into the tree.
        With an empty `tag` (release.yaml by hand on a branch, which releases nothing) the
        version is the tree's, `0.0.0-dev`, and the notes say so.
    canary.py stamp '{"tag": "v0.YYYYMMDD.N"}'
        The tag's version written into Cargo.toml and the workspace's own entries of Cargo.lock,
        in this checkout only, for what needs a manifest's version to be the release's
        (publishing to crates.io). Never committed: main's workspace version stays `0.0.0`, so a
        release changes no crate's identity and no build of main misses blaze's store.

No commit goes to main, so a cut never races a merge, and a canary is the bytes of a commit
that main's gate built, as it is. train_release.py's numbering counts every tag, as `cut`'s does,
so the two never take the same version, and `cut` cuts nothing while release-cut.yaml runs.

The JSON is inline, a file's path, or `-` for stdin. Needs git (a checkout of main with its
history and tags) and gh (GH_TOKEN with contents: write and actions: write for `cut` with
`push`). Python 3.11+, stdlib.
"""
import asyncio, datetime, json, os, re, subprocess, sys, time, typing
from dataclasses import dataclass, fields
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

ROOT = Path(__file__).resolve().parents[2]
REPO = os.environ.get("GITHUB_REPOSITORY", "indexable-inc/blaze")
DATED = re.compile(r"v0\.(\d{8})\.(0|[1-9]\d*)")
# A change to nothing but these makes no canary: blaze is the same program.
DOCS = re.compile(r"^(docs/|site/|tools/agents/|tools/bench/results/)|\.md$")
# The workspace's version in Cargo.toml: `0.0.0` on main, the tree's (a build of it says
# `0.0.0-dev`); `stamp` writes a release's into a checkout.
TREE_VERSION = "0.0.0"
WORKSPACE_VERSION = re.compile(r'(\[workspace\.package\]\nversion = ")([^"]+)"')
# How many of main's newest commits `cut` looks through for a built one.
LOOK_BACK = 40
# The merge gate, and the job of its run on a push to main that a canary needs to have passed.
GATE = ".github/workflows/blaze.yaml"
BUILD = "linux x64 / build"
# How often a wait asks again.
POLL_SECS = 60
# How long a pushed commit's gate run may take to appear before the push counts as having none.
RUN_GRACE_SECS = 300
STARTED = time.monotonic()


@dataclass(frozen=True)
class Cut:
    push: bool = False
    min_minutes: int = 30
    # A commit of main just pushed (canary.yaml on a push), or empty: no waiting.
    wait: str = ""
    wait_minutes: int = 90


@dataclass(frozen=True)
class Notes:
    tag: str
    notes: str


@dataclass(frozen=True)
class Stamp:
    tag: str


def typed(cls, text):
    """`cls` from the JSON `text` (inline, a path, or `-`); exits naming the field that is wrong."""
    if text == "-":
        text = sys.stdin.read()
    elif text and not text.lstrip().startswith("{"):
        text = open(text).read()
    try:
        value = json.loads(text or "{}")
    except json.JSONDecodeError as e:
        sys.exit(f"canary.py: the input is not JSON: {e}")
    if not isinstance(value, dict):
        sys.exit("canary.py: the input is a JSON object")
    known = {f.name for f in fields(cls)}
    if unknown := sorted(set(value) - known):
        sys.exit(f"canary.py: no field {', '.join(unknown)} (fields: {', '.join(sorted(known))})")
    hints = typing.get_type_hints(cls)
    for name, given in value.items():
        hint = hints[name]
        # A bool is an int to isinstance.
        if not isinstance(given, hint) or (hint is int and isinstance(given, bool)):
            sys.exit(f"canary.py: field {name} is a {hint.__name__}, not {given!r}")
    try:
        return cls(**value)
    except TypeError as e:
        sys.exit(f"canary.py: {e}")


def run(*argv, check=True):
    done = subprocess.run(argv, cwd=ROOT, capture_output=True, text=True)
    if check and done.returncode:
        sys.exit(f"canary.py: `{' '.join(argv)}` exited {done.returncode}: {done.stderr.strip()[-1500:]}")
    return done.stdout.strip()


def output(name, value):
    if path := os.environ.get("GITHUB_OUTPUT"):
        with open(path, "a") as f:
            f.write(f"{name}={value}\n")


def summary(line):
    print(line, flush=True)
    if path := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(path, "a") as f:
            f.write(f"- {line}\n")


def dated_tags():
    return [t for t in run("git", "tag", "-l", "v0.*").split() if DATED.fullmatch(t)]


def previous(rev, but=None):
    """The newest dated tag on `rev`'s history (`but` left out), or None."""
    argv = ["git", "describe", "--tags", "--abbrev=0", "--match", "v0.*", rev]
    if but:
        argv[-1:-1] = ["--exclude", but]
    found = run(*argv, check=False)
    return found if DATED.fullmatch(found or "") else None


def gate_run(sha):
    """The merge gate's run (blaze.yaml) on the push of `sha` to main, or None. A re-run keeps
    its id; the newest run is taken if there are several."""
    found = json.loads(run("gh", "api", f"repos/{REPO}/actions/runs?head_sha={sha}&event=push&branch=main&per_page=20"))
    runs = [r for r in found["workflow_runs"] if r["path"] == GATE]
    return max(runs, key=lambda r: r["id"]) if runs else None


def build(sha, pushed=False):
    """Where main's build of `sha` is: "built" (`linux x64 / build` passed), "failed",
    "building", or "none" (the gate builds nothing for it: a change to nothing the build reads,
    or no gate run, which a commit pushed by a workflow's own token has). With `pushed`, `sha`
    was just pushed and its run may not be there yet: no run is "building" for the first
    `RUN_GRACE_SECS` of the wait, and "none" after."""
    gate = gate_run(sha)
    if gate is None:
        return ("building" if pushed and time.monotonic() < STARTED + RUN_GRACE_SECS else "none"), None
    jobs = {j["name"]: j for j in json.loads(run("gh", "api", f"repos/{REPO}/actions/runs/{gate['id']}/jobs?filter=latest&per_page=100"))["jobs"]}
    if job := jobs.get(BUILD):
        if job["status"] != "completed":
            return "building", gate
        return ("built" if job["conclusion"] == "success" else "failed"), gate
    # A skipped `linux x64` (blaze.yaml's call of blaze-platform.yaml) has no jobs of its own.
    if gate["status"] == "completed" or jobs.get("linux x64", {}).get("status") == "completed":
        return "none", gate
    return "building", gate


def wait_for_build(since, pushed, minutes):
    """Until the newest of `since` (main's commits, newest first) that has a build has finished
    it: what the cut then takes is the newest build of main, not one older because the newest
    was a minute short. At most `minutes`, then an error naming the build."""
    deadline = time.monotonic() + minutes * 60
    while True:
        for sha in since:
            state, gate = build(sha, pushed=sha == pushed)
            if state != "none":
                break
        else:
            return
        if state != "building":
            return
        where = gate["html_url"] if gate else "its run has not started"
        if time.monotonic() >= deadline:
            sys.exit(f"canary.py: main's build of {sha[:10]} ({BUILD}) has not finished after {minutes} min: {where}")
        print(f"waiting for main's build of {sha[:10]} ({where})", flush=True)
        time.sleep(POLL_SECS)


def cut(args):
    import train_release
    main = run("git", "rev-parse", "origin/main")
    base = previous(main)
    # Main moves every few minutes and its build takes longer than that, so HEAD is seldom
    # built yet: the canary is the newest commit since the last release that main's gate built.
    since = run("git", "rev-list", "--first-parent", "--max-count", str(LOOK_BACK), f"{base}..{main}" if base else main).split()
    if not since:
        return summary(f"main {main[:10]} is {base} already")
    if args.wait:
        wait_for_build(since, args.wait, args.wait_minutes)
    head = next((sha for sha in since if build(sha)[0] == "built"), None)
    if head is None:
        return summary(f"none of main's {len(since)} newest commits since {base} has passed `{BUILD}` yet; no canary")
    if base:
        files = run("git", "diff", "--name-only", f"{base}..{head}").split()
        if not any(not DOCS.search(f) for f in files):
            return summary(f"nothing but docs from {base} to {head[:10]}; no canary")
        subjects = run("git", "log", "--first-parent", "--format=%s", f"{base}..{head}").splitlines()
        if all(train_release.BOT.search(s) for s in subjects):
            return summary(f"nothing from {base} to {head[:10]} but the bots' own; no canary")
    for workflow, doing in (("release-cut.yaml", "cutting a release"), ("release.yaml", "building")):
        running = json.loads(run("gh", "run", "list", "-R", REPO, "-w", workflow, "-L", "10", "--json", "status,headBranch"))
        # release.yaml on a branch builds and releases nothing: only a tag's run is a release.
        if busy := [r["headBranch"] for r in running if r["status"] in ("queued", "in_progress", "waiting", "pending", "requested")
                    and (workflow == "release-cut.yaml" or DATED.fullmatch(r["headBranch"]))]:
            return summary(f"{workflow} is {doing} ({', '.join(busy)}); the next push to main or the hourly run cuts the canary")
    releases = json.loads(run("gh", "release", "list", "-R", REPO, "-L", "10", "--json", "tagName,createdAt"))
    dated = [r for r in releases if DATED.fullmatch(r["tagName"])]
    if dated:
        newest = max(dated, key=lambda r: r["createdAt"])
        made = datetime.datetime.fromisoformat(newest["createdAt"].replace("Z", "+00:00"))
        age = (datetime.datetime.now(datetime.UTC) - made).total_seconds() / 60
        if age < args.min_minutes:
            return summary(f"{newest['tagName']} is {age:.0f} min old (fewer than {args.min_minutes}); no canary yet")
    version = train_release.next_version(dated_tags(), datetime.datetime.now(datetime.UTC).date())
    tag = f"v{version}"
    if not args.push:
        return summary(f"would tag {tag} at {head[:10]} ({since.index(head)} behind main, since {base}) and dispatch release.yaml")
    # Through the API with the workflow's own token, which starts no workflow on it: release.yaml
    # runs once, by the dispatch.
    run("gh", "api", f"repos/{REPO}/git/refs", "-f", f"ref=refs/tags/{tag}", "-f", f"sha={head}")
    run("gh", "workflow", "run", "release.yaml", "-R", REPO, "--ref", tag)
    summary(f"tagged {tag} at main {head[:10]} (since {base}) and dispatched release.yaml")


def unreleased(changelog):
    """The body of CHANGELOG.md's [Unreleased] section."""
    head = re.search(r"^## \[Unreleased\][^\n]*\n", changelog, re.M)
    if not head:
        sys.exit("canary.py: CHANGELOG.md has no '## [Unreleased]' heading")
    rest = changelog[head.end():]
    end = re.search(r"^(## |\[[^\]]+\]: )", rest, re.M)
    return rest[: end.start() if end else len(rest)].strip()


def sections(body):
    """[Unreleased]'s body as {heading: lines}, the lines before any `### ` heading under ""."""
    found, heading = {}, ""
    for line in body.splitlines():
        if line.startswith("### "):
            heading = line
            found.setdefault(heading, [])
        elif line.strip():
            found.setdefault(heading, []).append(line)
    return found


def added(now, then):
    """What [Unreleased] gained since the previous release: per `### ` heading, the lines of
    `now` that the same heading of `then` does not have, under their heading. [Unreleased] is
    never emptied by a release (no release commits to main), so each release's notes are what
    was added to it since the one before."""
    before = sections(then)
    out = []
    for heading, lines in sections(now).items():
        had = set(before.get(heading, []))
        if new := [line for line in lines if line not in had]:
            out += ([heading, ""] if heading else []) + new + [""]
    return "\n".join(out).strip()


def inheriting():
    """The names of the workspace's crates whose version is the workspace's
    (`version.workspace = true`): the ones `stamp` moves. A path crate with its own version
    (`0.0.0` for most of the front end's) keeps it."""
    import tomllib
    names = set()
    for manifest in ROOT.glob("crates/*/Cargo.toml"):
        package = tomllib.loads(manifest.read_text()).get("package", {})
        if package.get("version") == {"workspace": True}:
            names.add(package["name"])
    if not names:
        sys.exit("canary.py: no crate under crates/ has `version.workspace = true`")
    return names


def lock_stamped(text, old, new, names):
    """Cargo.lock with the workspace's own packages (no `source`) named in `names` at `old`
    moved to `new`."""
    blocks = text.split("\n[[package]]\n")
    for i, block in enumerate(blocks[1:], 1):
        name = re.search(r'^name = "([^"]+)"$', block, re.M)
        if not re.search(r"^source = ", block, re.M) and name and name[1] in names:
            blocks[i] = re.sub(rf'^version = "{re.escape(old)}"$', f'version = "{new}"', block, count=1, flags=re.M)
    return "\n[[package]]\n".join(blocks)


def tree_version():
    """Cargo.toml's workspace version, which must be the tree's `0.0.0`."""
    found = WORKSPACE_VERSION.search((ROOT / "Cargo.toml").read_text())
    if not found:
        sys.exit("canary.py: Cargo.toml has no `version` right under [workspace.package]")
    if found[2] != TREE_VERSION:
        sys.exit(f"canary.py: Cargo.toml's workspace version is {found[2]}, and the tree's is {TREE_VERSION}: "
                 "a release's version is never committed (crates/blaze-engine/build.rs, BLAZE_RELEASE)")
    return found


def notes(args):
    tree_version()
    head = run("git", "rev-parse", "HEAD")
    if not args.tag:
        ref = os.environ.get("GITHUB_REF_NAME", "this checkout")
        Path(args.notes).write_text(f"A build of {ref} at {head[:10]}, not a release.\n")
        output("version", f"{TREE_VERSION}-dev")
        return
    if not DATED.fullmatch(args.tag):
        sys.exit(f"canary.py: field tag is a dated release v0.YYYYMMDD.N or empty, not {args.tag!r}")
    import train_release
    base = previous(args.tag, but=args.tag)
    titles = asyncio.run(train_release.changes(base)) if base else []
    merged = "\n".join(f"- {title}" for title, _ in titles) or "- (none but the bots' own)"
    server = os.environ.get("GITHUB_SERVER_URL", "https://github.com")
    since = f" since [{base}]({server}/{REPO}/compare/{base}...{args.tag})" if base else ""
    body = [f"main at {head[:10]}, as it is.", ""]
    now = unreleased((ROOT / "CHANGELOG.md").read_text())
    then = unreleased(run("git", "show", f"{base}:CHANGELOG.md")) if base else ""
    if new := added(now, then):
        body += [new, ""]
    body += [f"### Merged{since}", "", merged, ""]
    Path(args.notes).write_text("\n".join(body))
    print(f"{args.tag}: {len(titles)} changes since {base}", flush=True)
    output("version", args.tag[1:])


def stamp(args):
    if not DATED.fullmatch(args.tag):
        sys.exit(f"canary.py: field tag is a dated release v0.YYYYMMDD.N, not {args.tag!r}")
    new = args.tag[1:]
    found = tree_version()
    manifest = ROOT / "Cargo.toml"
    text = manifest.read_text()
    manifest.write_text(text[:found.start(2)] + new + text[found.end(2):])
    lock = ROOT / "Cargo.lock"
    lock.write_text(lock_stamped(lock.read_text(), TREE_VERSION, new, inheriting()))
    print(f"stamped {new} over {TREE_VERSION} in this checkout's Cargo.toml and Cargo.lock", flush=True)


COMMANDS = {"cut": (Cut, cut), "notes": (Notes, notes), "stamp": (Stamp, stamp)}


def main():
    words = sys.argv[1:]
    if len(words) not in (1, 2) or words[0] not in COMMANDS:
        sys.exit(__doc__)
    cls, command = COMMANDS[words[0]]
    command(typed(cls, words[1] if len(words) == 2 else ""))


if __name__ == "__main__":
    main()
