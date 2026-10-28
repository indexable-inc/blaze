#!/usr/bin/env python3
"""canary.py: a canary of every green main, cut by a tag on main's own commit (docs/release.md).

    canary.py cut '{"push": false, "min_minutes": 30}'
        the newest commit of main (first parents, since the newest release) that passed the
        merge gate (its `ci` check, from the push run on main), when it has changes since that
        release beyond the bots' own and the docs: the tag v0.YYYYMMDD.N on that commit, made
        through the API, and release.yaml dispatched on it.
        Nothing when a release was published less than `min_minutes` ago or release.yaml is
        running: the next green main, or the hourly run, cuts it then. Without `push`, what it
        would tag is shown and nothing leaves the checkout.
    canary.py stamp '{"tag": "v0.YYYYMMDD.N", "notes": "<file>"}'
        release.yaml, in its checkout of the tag: when the workspace version is not the tag's
        (a canary's tag is on a commit of main, not on a `release:` commit), the tag's version
        in Cargo.toml and the workspace's own entries of Cargo.lock, so that `blaze --version`
        and the archives' names are the tag's, and, with `notes`, the release notes: CHANGELOG.md's
        [Unreleased] and the title of each change merged since the previous release. When the
        version is the tag's (a release commit of train_release.py), the notes are
        release_notes.py's and nothing is stamped. Step output `version`.

No commit goes to main, so a cut never races a merge, and a canary is the bytes of a commit
that passed the gate, as it is. Main's Cargo.toml keeps the version of the last release commit;
train_release.py's numbering counts every tag, so the two never take the same version, and
`cut` cuts nothing while release-cut.yaml runs.

The JSON is inline, a file's path, or `-` for stdin. Needs git (a checkout of main with its
history and tags) and gh (GH_TOKEN with contents: write and actions: write for `cut` with
`push`). Python 3.11+, stdlib; `stamp` without notes runs on 3.10 too (release.yaml's
ubuntu-22.04).
"""
import asyncio, datetime, json, os, re, subprocess, sys, typing
from dataclasses import dataclass, fields
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

ROOT = Path(__file__).resolve().parents[2]
REPO = os.environ.get("GITHUB_REPOSITORY", "indexable-inc/blaze")
DATED = re.compile(r"v0\.(\d{8})\.(0|[1-9]\d*)")
# A change to nothing but these makes no canary: blaze is the same program.
DOCS = re.compile(r"^(docs/|site/|tools/agents/|tools/bench/results/)|\.md$")
# release.yaml's builds run on ubuntu-22.04, whose Python (3.10) has no tomllib: `stamp` without
# notes needs nothing newer, and train_release.py (which imports tomllib) is imported where used.
WORKSPACE_VERSION = re.compile(r'(\[workspace\.package\]\nversion = ")([^"]+)"')
# How many of main's newest commits `cut` looks through for a green one.
LOOK_BACK = 40


@dataclass(frozen=True)
class Cut:
    push: bool = False
    min_minutes: int = 30


@dataclass(frozen=True)
class Stamp:
    tag: str
    notes: str = ""


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


def green(sha):
    checks = json.loads(run("gh", "api", f"repos/{REPO}/commits/{sha}/check-runs?check_name=ci&per_page=20"))
    return any((c["status"], c["conclusion"]) == ("completed", "success") for c in checks["check_runs"])


def cut(args):
    import train_release
    main = run("git", "rev-parse", "origin/main")
    base = previous(main)
    # Main moves every few minutes and its gate takes longer than that, so HEAD is seldom green
    # yet: the canary is the newest commit since the last release whose gate is.
    since = run("git", "rev-list", "--first-parent", "--max-count", str(LOOK_BACK), f"{base}..{main}" if base else main).split()
    if not since:
        return summary(f"main {main[:10]} is {base} already")
    head = next((sha for sha in since if green(sha)), None)
    if head is None:
        return summary(f"none of main's {len(since)} newest commits since {base} has a green `ci` yet; no canary")
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
            return summary(f"{workflow} is {doing} ({', '.join(busy)}); the next green main or the hourly run cuts the canary")
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
        return ""
    rest = changelog[head.end():]
    end = re.search(r"^(## |\[[^\]]+\]: )", rest, re.M)
    return rest[: end.start() if end else len(rest)].strip()


def lock_stamped(text, old, new):
    """Cargo.lock with the workspace's own packages (no `source`) at `old` moved to `new`."""
    blocks = text.split("\n[[package]]\n")
    for i, block in enumerate(blocks[1:], 1):
        if not re.search(r"^source = ", block, re.M):
            blocks[i] = re.sub(rf'^version = "{re.escape(old)}"$', f'version = "{new}"', block, count=1, flags=re.M)
    return "\n[[package]]\n".join(blocks)


def stamp(args):
    if not DATED.fullmatch(args.tag):
        sys.exit(f"canary.py: field tag is a dated release v0.YYYYMMDD.N, not {args.tag!r}")
    new = args.tag[1:]
    manifest = ROOT / "Cargo.toml"
    text = manifest.read_text()
    found = WORKSPACE_VERSION.search(text)
    if not found:
        sys.exit("canary.py: Cargo.toml has no `version` right under [workspace.package]")
    old = found[2]
    if old == new:
        # A release commit's tag: CHANGELOG.md has its section.
        if args.notes:
            done = subprocess.run([sys.executable, "-I", str(ROOT / ".github/scripts/release_notes.py"),
                                   "--tag", args.tag, "--out", args.notes], cwd=ROOT)
            if done.returncode:
                sys.exit(done.returncode)
        else:
            output("version", new)
        return
    manifest.write_text(text[:found.start(2)] + new + text[found.end(2):])
    lock = ROOT / "Cargo.lock"
    lock.write_text(lock_stamped(lock.read_text(), old, new))
    print(f"stamped {new} over {old} in Cargo.toml and Cargo.lock", flush=True)
    if args.notes:
        head = run("git", "rev-parse", "HEAD")
        base = previous(args.tag, but=args.tag)
        import train_release
        titles = asyncio.run(train_release.changes(base)) if base else []
        merged = "\n".join(f"- {title}" for title, _ in titles) or "- (none but the bots' own)"
        server = os.environ.get("GITHUB_SERVER_URL", "https://github.com")
        since = f" since [{base}]({server}/{REPO}/compare/{base}...{args.tag})" if base else ""
        body = [f"A canary: main at {head[:10]}, which passed the merge gate, as it is.", ""]
        if notes := unreleased((ROOT / "CHANGELOG.md").read_text()):
            body += [notes, ""]
        body += [f"### Merged{since}", "", merged, ""]
        Path(args.notes).write_text("\n".join(body))
    output("version", new)


def main():
    words = sys.argv[1:]
    if len(words) not in (1, 2) or words[0] not in ("cut", "stamp"):
        sys.exit(__doc__)
    given = words[1] if len(words) == 2 else ""
    if words[0] == "cut":
        cut(typed(Cut, given))
    else:
        stamp(typed(Stamp, given))


if __name__ == "__main__":
    main()
