#!/usr/bin/env python3
"""train_release.py: a release of main as it is, by a tag on main's own commit.

    train_release.py '{"push": true}'   (train.yaml after a green train, release-cut.yaml by hand)

Versions are dated, and still semver, as cargo and crates.io need: `0.YYYYMMDD.N`, N counting
the releases of that day (UTC) from 0, so 0.20261026.0, then 0.20261026.1; the tag is
`v0.20261026.0`, and `blaze --version` says `blaze 0.20261026.0 (fb82b09 2026-10-26)`.

When main has changes since the last release (the newest dated tag on HEAD's history) beyond
the trains' and the bots' own (`fmt`, `pgo: ...`, `release: ...`): the tag `v0.YYYYMMDD.N` of
today's next release on HEAD, made through the API, and release.yaml dispatched on it (a tag
made with the workflow's own token starts no workflow). No commit goes to main: the
workspace's version in Cargo.toml stays `0.0.0`, and the release build is told the release by
`BLAZE_RELEASE` (crates/blaze-engine/build.rs). A version committed to main would change every
crate's identity (`CARGO_PKG_VERSION`), and so every build of main after a release would miss
blaze's store. The notes are release.yaml's (`canary.py notes`).

Without `push`, the tag it would make is shown and nothing leaves the checkout. The JSON is
inline, a file's path, or `-` for stdin. Needs git (a checkout with history and tags) and gh
(GH_TOKEN with contents: write and actions: write). Python 3.11+, stdlib.
"""
import asyncio, datetime, json, os, re, sys
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
REPO = os.environ.get("GITHUB_REPOSITORY", "indexable-inc/blaze")
BOT = re.compile(r"^(fmt$|pgo: |release: v|Merge remote-tracking branch )|\[skip ci\]")
MERGE = re.compile(r"^Merge (?:pull request #\d+ from \S+|[^:\s]+: )")
DATED = re.compile(r"v0\.\d{8}\.(0|[1-9]\d*)")


@dataclass(frozen=True)
class Args:
    push: bool = False


def typed(text):
    """`Args` from the JSON `text` (inline, a path, or `-`); exits naming the field that is wrong."""
    if text == "-":
        text = sys.stdin.read()
    elif text and not text.lstrip().startswith("{"):
        text = Path(text).read_text()
    try:
        value = json.loads(text or "{}")
    except json.JSONDecodeError as e:
        sys.exit(f"train_release.py: the input is not JSON: {e}")
    if not isinstance(value, dict):
        sys.exit("train_release.py: the input is a JSON object")
    if unknown := sorted(set(value) - {"push"}):
        sys.exit(f"train_release.py: no field {', '.join(unknown)} (fields: push)")
    if not isinstance(value.get("push", False), bool):
        sys.exit(f"train_release.py: field push is a bool, not {value['push']!r}")
    return Args(**value)


class StepFailed(Exception):
    pass


async def run(*argv, step, timeout=120, cwd=ROOT, check=True):
    """The command's stdout; a failure names the step, the command, its exit code and stderr's tail."""
    proc = await asyncio.create_subprocess_exec(*argv, cwd=cwd, stdout=asyncio.subprocess.PIPE,
                                                stderr=asyncio.subprocess.PIPE)
    try:
        async with asyncio.timeout(timeout):
            out, err = await proc.communicate()
    except TimeoutError:
        proc.kill()
        raise StepFailed(f"{step}: `{' '.join(argv)}` took more than {timeout} s") from None
    if check and proc.returncode:
        tail = err.decode(errors="replace").strip()[-1500:]
        raise StepFailed(f"{step}: `{' '.join(argv)}` exited {proc.returncode}: {tail}")
    return out.decode(errors="replace")


def next_version(tags, today):
    """`0.YYYYMMDD.N` for the next release of `today` (a date), given the tags there are."""
    day = today.strftime("%Y%m%d")
    taken = [int(m[1]) for t in tags if (m := re.fullmatch(rf"v0\.{day}\.(\d+)", t))]
    return f"0.{day}.{max(taken) + 1 if taken else 0}"


async def changes(base):
    """The title of each change on main since `base`, oldest first, as (title, sha)."""
    log = await run("git", "log", "--first-parent", "--reverse", "--format=%H%x00%s%x00%b%x1e",
                    f"{base}..HEAD", step="the commits since the last release")
    found = []
    for record in filter(str.strip, log.split("\x1e")):
        sha, subject, body = record.strip("\n").split("\x00", 2)
        if BOT.search(subject):
            continue
        title = subject
        if subject.startswith("Merge pull request #"):
            # GitHub's own merge message: the title is the body's first line.
            number = re.match(r"Merge pull request (#\d+)", subject)[1]
            first = next((line for line in body.splitlines() if line.strip()), "")
            title = f"{first.strip()} ({number})" if first else subject
        else:
            title = MERGE.sub("", subject)
        found.append((title, sha))
    return found


async def release(args):
    base = (await run("git", "describe", "--tags", "--abbrev=0", "--match", "v0.*", "HEAD",
                      step="the last release", check=False)).strip()
    if not DATED.fullmatch(base):
        raise StepFailed(f"the last release: no dated tag v0.YYYYMMDD.N on HEAD's history (git describe said {base!r})")
    titles = await changes(base)
    if not titles:
        print(f"nothing to release: no change since {base} but the bots' own", flush=True)
        return [f"nothing to release since {base}"]
    tags = (await run("git", "tag", "-l", "v*", step="the release tags")).split()
    # The newest release by number must be on HEAD's history: a canary (canary.yaml) may have
    # tagged a newer commit of main since this checkout's, and a higher number on older code
    # would move the canary channel back to it.
    newest = max((t for t in tags if DATED.fullmatch(t)), key=lambda t: tuple(int(n) for n in t[1:].split(".")))
    if newest != base:
        on_history = (await run("git", "tag", "--merged", "HEAD", "-l", newest, step="the newest release")).strip()
        if on_history != newest:
            print(f"nothing to release: {newest}, the newest release, is not on HEAD's history (a newer commit of main has it)", flush=True)
            return [f"nothing to release: {newest} is newer than HEAD"]
    tag = f"v{next_version(tags, datetime.datetime.now(datetime.UTC).date())}"
    head = (await run("git", "rev-parse", "HEAD", step="HEAD")).strip()
    if not args.push:
        print(f"would tag {tag} at {head} ({len(titles)} changes since {base})", flush=True)
        return [f"would tag {tag} at {head[:10]}: {len(titles)} changes since {base}"]
    # Made through the API with GH_TOKEN, the workflow's own token, which starts no workflow on
    # it: release.yaml runs once, by the dispatch.
    await run("gh", "api", f"repos/{REPO}/git/refs", "-f", f"ref=refs/tags/{tag}", "-f", f"sha={head}",
              step="the tag")
    await run("gh", "workflow", "run", "release.yaml", "-R", REPO, "--ref", tag, step="dispatch release.yaml")
    return [f"tagged {tag} at {head[:10]} ({len(titles)} changes since {base}) and dispatched release.yaml"]


async def main():
    words = sys.argv[1:]
    if len(words) > 1:
        sys.exit(__doc__)
    args = typed(words[0] if words else "")
    try:
        lines = await release(args)
    except StepFailed as error:
        sys.exit(f"train_release.py: {error}")
    if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(summary, "a") as f:
            f.write("".join(f"- {line}\n" for line in lines))


if __name__ == "__main__":
    asyncio.run(main())
