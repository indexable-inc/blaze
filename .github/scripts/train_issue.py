#!/usr/bin/env python3
"""train_issue.py: the release train's one issue, opened, added to or closed.

    python3 .github/scripts/train_issue.py red     a red train: open the issue, or comment on it
    python3 .github/scripts/train_issue.py green   a green train: close the issue, if it is open

Run by .github/actions/train-issue in train.yaml's last job, with GH_TOKEN (issues: write,
actions: read), GITHUB_REPOSITORY, GITHUB_RUN_ID, GITHUB_RUN_ATTEMPT and GITHUB_SHA set, as
Actions sets them. One issue, titled TITLE, for as long as main is red: each red train is a
comment on it while it is open, with the run, the jobs that failed (and for a harness part, the
harnesses: ci.py's `failed:` line from the job's log), and the commits since the last green
train, which are where the failure came in. A green train closes it. Jobs that only gather the
others are left out, and so are the ones that do not block the train (NOT_BLOCKING, the nix
jobs and the bench), which are listed apart. Python 3.11+, stdlib and the `gh` CLI.
"""
import asyncio, json, os, re, sys

REPO = os.environ["GITHUB_REPOSITORY"]
RUN = os.environ["GITHUB_RUN_ID"]
ATTEMPT = os.environ.get("GITHUB_RUN_ATTEMPT", "1")
SHA = os.environ["GITHUB_SHA"]
SERVER = os.environ.get("GITHUB_SERVER_URL", "https://github.com")
RUN_URL = f"{SERVER}/{REPO}/actions/runs/{RUN}"
TITLE = "Release train is red"
# The job of train.yaml whose success is the train's: the one that gathers the blocking jobs.
VERDICT = "train"
GATHERERS = {VERDICT, "train issue", "release"}
NOT_BLOCKING = ("nix / ", "linux x64 / bench")
FAILED = {"failure", "cancelled", "timed_out"}
ANSI = re.compile(r"\x1b\[[0-9;]*m")


class GhFailed(Exception):
    pass


async def gh(*args, step):
    proc = await asyncio.create_subprocess_exec("gh", *args, stdout=asyncio.subprocess.PIPE,
                                                stderr=asyncio.subprocess.PIPE)
    try:
        async with asyncio.timeout(120):
            out, err = await proc.communicate()
    except TimeoutError:
        proc.kill()
        raise GhFailed(f"{step}: gh {' '.join(args[:3])} took more than 120 s") from None
    if proc.returncode:
        tail = err.decode(errors="replace").strip()[-800:]
        raise GhFailed(f"{step}: gh {' '.join(args[:3])} exited {proc.returncode}: {tail}")
    return out.decode(errors="replace")


async def jobs_of(run, attempt=None):
    path = f"repos/{REPO}/actions/runs/{run}" + (f"/attempts/{attempt}" if attempt else "") + "/jobs?per_page=100"
    out = await gh("api", "--paginate", path, "--jq", ".jobs[]", step=f"the jobs of run {run}")
    return [json.loads(line) for line in out.splitlines() if line.strip()]


async def failed_harnesses(job_id):
    """ci.py's `failed:` line of a harness job's log, or None for a job with none."""
    # gh refuses a response with terminal escapes unless told to, and a job's log has them.
    log = await gh("api", "--allow-escape-sequences", f"repos/{REPO}/actions/jobs/{job_id}/logs",
                   step=f"the log of job {job_id}")
    found = re.findall(r"failed: (.+)$", ANSI.sub("", log), re.M)
    return found[-1].strip() if found and found[-1].strip() != "none" else None


async def last_green():
    """The commit of the newest train on main before this one whose verdict job passed, or None."""
    out = await gh("api", f"repos/{REPO}/actions/runs/{RUN}", "--jq", ".workflow_id", step="this run's workflow")
    runs = json.loads(await gh("api", f"repos/{REPO}/actions/workflows/{out.strip()}/runs?branch=main&status=completed&per_page=30",
                               "--jq", "[.workflow_runs[] | {id, head_sha}]", step="the train's runs"))
    for run in runs:
        if str(run["id"]) == RUN:
            continue
        if any(j["name"] == VERDICT and j["conclusion"] == "success" for j in await jobs_of(run["id"])):
            return run["head_sha"]
    return None


async def open_issue():
    listed = await gh("issue", "list", "-R", REPO, "--state", "open", "--search", f'"{TITLE}" in:title',
                      "--json", "number,title", step="the open issues")
    return next((it["number"] for it in json.loads(listed) if it["title"] == TITLE), None)


async def red():
    jobs = await jobs_of(RUN, ATTEMPT)
    failed = [j for j in jobs if j["conclusion"] in FAILED and j["name"] not in GATHERERS]
    blocking = [j for j in failed if not j["name"].startswith(NOT_BLOCKING)]
    soft = [j for j in failed if j["name"].startswith(NOT_BLOCKING)]
    release = [j for j in jobs if j["name"] == "release" and j["conclusion"] in FAILED]

    async def line(job):
        text = f"- [{job['name']}]({job['html_url']}): {job['conclusion']}"
        if job["conclusion"] == "failure" and "harness" in job["name"]:
            if harnesses := await failed_harnesses(job["id"]):
                text += f"; the harnesses that failed: {harnesses}"
        return text

    async with asyncio.TaskGroup() as group:
        hard = [group.create_task(line(j)) for j in blocking]
        rest = [group.create_task(line(j)) for j in soft + release]
        green = group.create_task(last_green())
    lines = [f"The train at {SHA[:10]} is red: {RUN_URL}", ""]
    if hard:
        lines += ["The jobs that failed:", *(t.result() for t in hard), ""]
    if rest:
        lines += ["Also failed (not blocking the train, or its release):", *(t.result() for t in rest), ""]
    if green.result():
        base = green.result()
        lines.append(f"Since the last green train: {SERVER}/{REPO}/compare/{base[:10]}...{SHA[:10]}")
    else:
        lines.append("No green train on main in the last 30 runs.")
    body = "\n".join(lines)
    number = await open_issue()
    if number:
        await gh("issue", "comment", str(number), "-R", REPO, "--body", body, step=f"a comment on #{number}")
        print(f"#{number}: commented\n{body}")
    else:
        url = await gh("issue", "create", "-R", REPO, "--title", TITLE, "--body", body, step="the issue")
        print(f"{url.strip()}: opened\n{body}")


async def green():
    number = await open_issue()
    if not number:
        print("no open issue; nothing to close")
        return
    await gh("issue", "close", str(number), "-R", REPO, "--comment", f"The train at {SHA[:10]} is green: {RUN_URL}",
             step=f"closing #{number}")
    print(f"#{number}: closed")


def main():
    if sys.argv[1:] not in (["red"], ["green"]):
        sys.exit(__doc__)
    try:
        asyncio.run(red() if sys.argv[1] == "red" else green())
    except (GhFailed, ExceptionGroup) as error:
        sys.exit(f"train_issue.py {sys.argv[1]}: {error!r}")


if __name__ == "__main__":
    main()
