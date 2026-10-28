#!/usr/bin/env python3
"""train_release.py: the release train's release, in two steps, once main is green.

    train_release.py prepare [--push]   (train.yaml, after a green train)
    train_release.py tag [--push]       (prepare --push runs it; PGO is off for now)

Versions are dated, and still semver, as cargo and crates.io need: `0.YYYYMMDD.N`, N counting
the releases of that day (UTC) from 0, so 0.20261026.0, then 0.20261026.1; the tag is
`v0.20261026.0`, and `blaze --version` says `blaze 0.20261026.0 (fb82b09)` (crates/blaze-engine's
build.rs adds the commit).

`prepare`: when main has commits since the last release, the workspace version of today's next
release, a CHANGELOG.md section for it (what [Unreleased] had, and the title of each change merged since
the last release), Cargo.lock and tools/nix/units for the new version, all in one commit `release:
v0.YYYYMMDD.N [skip ci]` on main; then the pgo workflow, by dispatch. A release is built against
profiles trained on its own crates (`build_release.py --strict`), and the version is part of
every crate's hash, so the tag waits for the profiles. Nothing to release when the commits
since are the trains' and the bots' own (`fmt`, `pgo: ...`, `release: ...`).

`tag`: when the workspace version is a release `prepare` made and has no tag yet, the tag
`v0.YYYYMMDD.N` on HEAD (the commit that pins its profiles), and release.yaml on that tag, by
dispatch: a tag made with the workflow's own token starts no workflow.

While a release waits for its profiles, `prepare` does nothing new: it says so, and fails when
the pgo workflow's last run on main since the release commit failed, so that the train's issue
shows it. The last release is the newest tag `v*`; before the first, the commit that added the
workspace version's CHANGELOG.md section.

Without --push nothing leaves the checkout: the commit or tag is made and shown. Needs git, gh
(GH_TOKEN with contents: write and actions: write), and for `prepare` nix (the flake's dev shell
runs cargo and tools/nix/unit_graph.py). Python 3.11+, stdlib.
"""
import argparse, asyncio, datetime, os, re, sys, tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
REPO = os.environ.get("GITHUB_REPOSITORY", "indexable-inc/blaze")
SERVER = os.environ.get("GITHUB_SERVER_URL", "https://github.com")
BOT = re.compile(r"^(fmt$|pgo: |release: v|Merge remote-tracking branch )|\[skip ci\]")
MERGE = re.compile(r"^Merge (?:pull request #\d+ from \S+|[^:\s]+: )")


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


def workspace_version():
    return tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]


def key(version):
    return tuple(int(n) for n in version.split("."))


def next_version(tags, today):
    """`0.YYYYMMDD.N` for the next release of `today` (a date), given the tags there are."""
    day = today.strftime("%Y%m%d")
    taken = [int(m[1]) for t in tags if (m := re.fullmatch(rf"v0\.{day}\.(\d+)", t))]
    return f"0.{day}.{max(taken) + 1 if taken else 0}"


async def state():
    """(version, base, waiting): the workspace version; the commit the last release was made
    at; and the release commit of that version when it is not tagged yet (None otherwise)."""
    version = workspace_version()
    tags = (await run("git", "tag", "-l", "v*", step="the release tags")).split()
    released = sorted((t for t in tags if re.fullmatch(r"v\d+\.\d+\.\d+", t)), key=lambda t: key(t[1:]))
    if f"v{version}" in released:
        return version, f"v{version}", None
    made = (await run("git", "log", "-1", "--format=%H", "--fixed-strings", f"--grep=release: v{version} ",
                      "HEAD", step="the release commit")).strip()
    if made:
        return version, None, made
    if released:
        # The version moved by hand past the last tag: what is since that tag is this version's.
        return version, released[-1], None
    added = (await run("git", "log", "--format=%H", "-S", f"## [{version}]", "--", "CHANGELOG.md",
                       step="the commit that added the version's section")).split()
    if not added:
        raise StepFailed(f"the last release: no tag v* and no '## [{version}]' in CHANGELOG.md's history")
    return version, added[-1], None


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


def changelog(text, new, titles, today):
    """CHANGELOG.md with [Unreleased]'s body and the titles as the section of `new`."""
    head = re.search(r"^## \[Unreleased\][^\n]*\n", text, re.M)
    if not head:
        raise StepFailed("CHANGELOG.md has no '## [Unreleased]' heading")
    rest = text[head.end():]
    end = re.search(r"^(## |\[[^\]]+\]: )", rest, re.M)
    cut = end.start() if end else len(rest)
    unreleased, after = rest[:cut].strip(), rest[cut:]
    merged = "\n".join(f"- {title}" for title in titles)
    body = (unreleased + "\n\n" if unreleased else "") + f"### Merged\n\n{merged}"
    section = f"## [Unreleased]\n\n## [{new}] - {today}\n\n{body}\n\n"
    text = text[:head.start()] + section + after
    url = f"{SERVER}/{REPO}"
    text = re.sub(r"^\[Unreleased\]: .*$", f"[Unreleased]: {url}/compare/v{new}...HEAD\n"
                  f"[{new}]: {url}/releases/tag/v{new}", text, count=1, flags=re.M)
    return text


async def push_main(*, step):
    """HEAD onto main, rebased onto whatever was pushed since, five tries."""
    for attempt in range(1, 6):
        await run("git", "pull", "-q", "--rebase", "origin", "main", step=f"{step}: rebase onto main", timeout=300)
        try:
            await run("git", "push", "origin", "HEAD:main", step=step, timeout=300)
            return
        except StepFailed as error:
            if attempt == 5:
                raise
            print(f"{error}\ntrying again ({attempt}/5)", flush=True)
            await asyncio.sleep(10 * attempt)



async def prepare(args):
    version, base, waiting = await state()
    summary = []
    if waiting:
        # A release commit no tag has yet (one made while PGO gated releases): tag it now.
        print(f"v{version} (release commit {waiting[:10]}) has no tag yet", flush=True)
        return await tag(args)
    titles = await changes(base)
    if not titles:
        print(f"nothing to release: no change since {base} but the bots' own", flush=True)
        return [f"nothing to release since {base}"]
    now = datetime.datetime.now(datetime.UTC).date()
    tags = (await run("git", "tag", "-l", "v*", step="the release tags")).split()
    new = next_version(tags, now)
    today = now.isoformat()

    manifest = ROOT / "Cargo.toml"
    text = manifest.read_text()
    bumped = re.sub(r'(\[workspace\.package\]\nversion = ")[^"]+(")', rf"\g<1>{new}\g<2>", text, count=1)
    if bumped == text:
        raise StepFailed("Cargo.toml: no `version` right under [workspace.package]")
    manifest.write_text(bumped)
    log = ROOT / "CHANGELOG.md"
    log.write_text(changelog(log.read_text(), new, [t for t, _ in titles], today))
    # The workspace's own entries of Cargo.lock, then the unit graphs, which hash Cargo.lock.
    await run("nix", "develop", "--command", "cargo", "update", "--workspace", step="cargo update --workspace", timeout=1800)
    await run(sys.executable, "tools/nix/unit_graph.py", step="tools/nix/unit_graph.py", timeout=3600)

    await run("git", "add", "-A", "Cargo.toml", "Cargo.lock", "CHANGELOG.md", "tools/nix/units", step="git add")
    message = f"release: v{new} [skip ci]\n\n{len(titles)} changes since {base}, through {(await run('git', 'rev-parse', '--short=10', 'HEAD', step='HEAD')).strip()}."
    await run("git", "commit", "-q", "-m", message, step="git commit")
    shown = await run("git", "show", "--stat", "HEAD", step="git show")
    print(shown, flush=True)
    summary += [f"release commit for v{new}: {len(titles)} changes since {base}"]
    if args.push:
        await push_main(step="push the release commit")
        # PGO is off for now (release.yaml builds with --no-pgo): the release commit is tagged
        # here, not by pgo.yaml's pin job.
        summary.append("pushed to main")
        summary += await tag(args)
    return summary


async def tag(args):
    version, base, waiting = await state()
    if not waiting:
        print(f"no release waits: v{version} is {'tagged' if base == f'v{version}' else 'not a release commit'}")
        return [f"no release waits for v{version}"]
    head = (await run("git", "rev-parse", "HEAD", step="HEAD")).strip()
    if not args.push:
        print(f"would tag v{version} at {head}")
        return [f"would tag v{version} at {head[:10]}"]
    # Made through the API with GH_TOKEN, the workflow's own token, which starts no workflow on
    # it: release.yaml runs once, by the dispatch. A tag pushed with the bot's token would start
    # it a second time.
    await run("gh", "api", f"repos/{REPO}/git/refs", "-f", f"ref=refs/tags/v{version}", "-f", f"sha={head}",
              step="the tag")
    await run("gh", "workflow", "run", "release.yaml", "-R", REPO, "--ref", f"v{version}", step="dispatch release.yaml")
    return [f"tagged v{version} at {head[:10]} and dispatched release.yaml"]


async def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("step", choices=["prepare", "tag"])
    parser.add_argument("--push", action="store_true", help="push the commit or tag and dispatch the next workflow")
    args = parser.parse_args()
    try:
        lines = await (prepare if args.step == "prepare" else tag)(args)
    except StepFailed as error:
        sys.exit(f"train_release.py {args.step}: {error}")
    if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(summary, "a") as f:
            f.write("".join(f"- {line}\n" for line in lines))


if __name__ == "__main__":
    asyncio.run(main())
