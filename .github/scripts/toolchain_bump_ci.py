#!/usr/bin/env python3
"""toolchain_bump_ci.py: the daily toolchain bump's steps (.github/workflows/toolchain-bump.yaml,
docs/toolchain.md "The daily bump"). blaze tracks the latest Rust nightly: each day the newest
nightly with every component blaze needs is pinned on the branch BRANCH, its pull request runs
the merge gate and the release train, and the train's verdict queues it or says what broke.

    prepare [--date D]      on BRANCH (the open bump's, with main merged in, or main's): run
                            harness/toolchain_bump.py, commit, push; outputs changed, pin
    answers FILE...         merge answers.py --write's rustc.json of each host into the tree's
    pr                      commit the answers, push, open or update the pull request, start
                            the train on BRANCH and on BASE, the main commit BRANCH is on
    base-run RUN            outputs run, sha: BASE's train that the train run RUN of BRANCH is
                            compared with, waited for
    verdict RUN             the train run RUN of BRANCH: no job or harness red that is green on
                            BASE's train (main's own are cancelled by the next merge more often
                            than not): queue the pull request; red on the first attempt: run
                            the failed jobs again; else comment and open or update the issue
    blocked FILE [--date D] open or update the issue with what FILE says failed

git pushes with the checkout's token, BLAZE_BOT_TOKEN, since a push with GITHUB_TOKEN starts no
workflow; gh (GH_TOKEN) is GITHUB_TOKEN, which may open and comment on pull requests and issues
where the bot's token may not. A pull request GITHUB_TOKEN opens starts no workflow either, so it
is opened before the bot pushes the answers, whose push runs the gate on it. Python 3.11+,
stdlib, git and the gh CLI.
"""
import argparse, datetime, json, os, re, subprocess, sys, time

REPO = os.environ.get("GITHUB_REPOSITORY", "indexable-inc/blaze")
SERVER = os.environ.get("GITHUB_SERVER_URL", "https://github.com")
RUN_URL = f"{SERVER}/{REPO}/actions/runs/{os.environ.get('GITHUB_RUN_ID', '')}"
ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BRANCH = "toolchain/bump"
# The train of the main commit the bump is on: what the bump's train is compared with.
BASE = "toolchain/base"
PIN = os.path.join(ROOT, "harness/toolchain")
PINS = os.path.join(ROOT, "crates/blaze-cargo/src/toolchain/pins.txt")
ANSWERS = os.path.join(ROOT, "crates/blaze-units/src/stdlib/rustc.json")
# The issue is one for as long as the bump is blocked, retitled to the nightly it is on.
TITLE = "Toolchain bump to {date} is blocked"
TITLED = re.compile(r"^Toolchain bump to (\d{4}-\d{2}-\d{2}) is blocked$")
# train.yaml's jobs that gather the others, and those that do not block it (train_issue.py).
GATHERERS = {"train", "train issue", "release"}
NOT_BLOCKING = ("nix / ", "linux x64 / bench")
FAILED = {"failure", "cancelled", "timed_out"}
# Attempts of the bump's train before red that main does not have blocks it.
ATTEMPTS = 2
ANSI = re.compile(r"\x1b\[[0-9;]*m")


class Failed(Exception):
    pass


def run(*argv, check=True, cwd=ROOT, env=None):
    # GitHub's API answers a 502 now and then, and to every try at a large page of a train's jobs.
    for wait in (10, 30, 60, None):
        done = subprocess.run(argv, cwd=cwd, capture_output=True, text=True, env=env)
        if not done.returncode or wait is None or argv[0] != "gh" or not re.search(r"HTTP 5\d\d", done.stderr):
            break
        time.sleep(wait)
    if check and done.returncode:
        raise Failed(f"`{' '.join(argv[:4])}` exited {done.returncode}:\n{(done.stderr or done.stdout)[-3000:]}")
    return done.stdout


def gh(*args, check=True):
    return run("gh", *args, check=check)


def output(**values):
    path = os.environ.get("GITHUB_OUTPUT")
    for key, value in values.items():
        print(f"{key}={value}")
        if path:
            open(path, "a").write(f"{key}={value}\n")


def pin():
    return open(PIN).read().strip()


def pins():
    lines = dict(l.split(" ", 1) for l in open(PINS).read().splitlines() if l and not l.startswith("#"))
    return lines["rustc"], lines["commit"]


def open_pr():
    found = json.loads(gh("pr", "list", "-R", REPO, "--head", BRANCH, "--state", "open",
                          "--json", "number,title,createdAt,url,headRefOid"))
    return found[0] if found else None


def days_behind(name, newest=None):
    date = datetime.date.fromisoformat(name.removeprefix("nightly-"))
    return ((newest or datetime.date.today()) - date).days


# ---------------------------------------------------------------------------------------------
# The issue


def find_issue():
    listed = json.loads(gh("issue", "list", "-R", REPO, "--state", "open", "--search",
                           '"Toolchain bump to" in:title', "--json", "number,title"))
    return next((it["number"] for it in listed if TITLED.match(it["title"])), None)


def blocked(date, body):
    """Opens the issue for the bump to `date`, or retitles and comments on the open one."""
    title = TITLE.format(date=date)
    main_pin = run("git", "show", "origin/main:harness/toolchain", check=False).strip() or pin()
    body += (f"\n\nmain is pinned to {main_pin}, {days_behind(main_pin)} days behind today. "
             f"The bump retries every day on top of main (docs/toolchain.md, \"The daily bump\"). Run: {RUN_URL}")
    number = find_issue()
    if number:
        gh("issue", "edit", str(number), "-R", REPO, "--title", title)
        gh("issue", "comment", str(number), "-R", REPO, "--body", body)
        print(f"#{number}: commented\n{body}")
    else:
        url = gh("issue", "create", "-R", REPO, "--title", title, "--body", body).strip()
        print(f"{url}: opened\n{body}")


def unblocked(date, why):
    number = find_issue()
    if number:
        gh("issue", "close", str(number), "-R", REPO, "--comment", f"The bump to {date} {why}: {RUN_URL}")
        print(f"#{number}: closed")


# ---------------------------------------------------------------------------------------------
# prepare


def prepare(date):
    run("git", "fetch", "-q", "origin", "main")
    pr = open_pr()
    if pr:
        run("git", "fetch", "-q", "origin", BRANCH)
        run("git", "checkout", "-q", "-B", BRANCH, f"origin/{BRANCH}")
        merged = run("git", "merge", "--no-edit", "-q", "origin/main", check=False)
        if run("git", "diff", "--name-only", "--diff-filter=U").strip():
            files = run("git", "diff", "--name-only", "--diff-filter=U").split()
            run("git", "merge", "--abort", check=False)
            raise Failed(f"merging main into the open bump {pr['url']} conflicts in: {', '.join(files)}. "
                         f"Resolve it on {BRANCH} by hand.{merged}")
        base = f"origin/{BRANCH}"
        # A bump open for a day without merging is visible, whatever its train said.
        opened = datetime.datetime.fromisoformat(pr["createdAt"].replace("Z", "+00:00"))
        if datetime.datetime.now(datetime.timezone.utc) - opened > datetime.timedelta(hours=20):
            blocked(pin().removeprefix("nightly-"),
                    f"The bump {pr['url']} ({pr['title']}) has been open since {pr['createdAt']} without "
                    "merging. Its checks and the train's verdict on it say why.")
    else:
        run("git", "checkout", "-q", "-B", BRANCH, "origin/main")
        base = "origin/main"
    was = pin()
    argv = [sys.executable, "harness/toolchain_bump.py", json.dumps({"date": date or ""})]
    bumped = subprocess.run(argv, cwd=ROOT, capture_output=True, text=True)
    print(bumped.stdout, bumped.stderr, sep="\n")
    if bumped.returncode:
        raise Failed(f"`{' '.join(argv[1:])}` failed, moving {was}:\n\n```\n{(bumped.stderr or bumped.stdout)[-4000:]}\n```")
    now = pin()
    run("git", "add", "-A")
    if run("git", "status", "--porcelain").strip():
        rustc, commit = pins()
        run("git", "commit", "-q", "-m", f"toolchain: pin {now}\n\nrustc {rustc}, rust-lang/rust {commit}.\n"
            "harness/toolchain_bump.py, run by .github/workflows/toolchain-bump.yaml.")
    changed = run("git", "rev-parse", "HEAD").strip() != run("git", "rev-parse", base).strip()
    if changed:
        # With no bump open the branch is the workflow's alone, left by a run that stopped.
        run("git", "push", "-q", *([] if pr else ["--force"]), "origin", f"HEAD:refs/heads/{BRANCH}")
        # library.py merge made the new nightly's snapshot of the standard library, the next merge's base.
        if run("git", "rev-parse", "-q", "--verify", "refs/heads/upstream/library", check=False).strip():
            run("git", "push", "-q", "origin", "refs/heads/upstream/library:refs/heads/upstream/library")
    output(changed=str(changed).lower(), pin=now, was=was)


# ---------------------------------------------------------------------------------------------
# answers


def answers(files):
    """One rustc.json from each host's: the same answers but for each host's target specs."""
    merged = None
    for path in files:
        data = json.load(open(path))
        if merged is None:
            merged = data
            continue
        if data["version"] != merged["version"]:
            raise Failed(f"{path} has the answers of rustc {data['version']}, the others of {merged['version']}")
        for target, its in data["targets"].items():
            mine = merged["targets"].setdefault(target, its)
            for key, value in its.items():
                if key == "target_spec_json":
                    mine[key].update(value)
                elif mine.get(key) != value:
                    raise Failed(f"{path}: {target}'s {key} differs from another host's")
    with open(ANSWERS, "w") as out:
        json.dump(merged, out, indent=1, sort_keys=True)
        out.write("\n")
    hosts = sorted({h for t in merged["targets"].values() for h in t["target_spec_json"]})
    print(f"answers of rustc {merged['version']}, target specs of {', '.join(hosts)}")


# ---------------------------------------------------------------------------------------------
# pr


def pr():
    name = pin()
    rustc, commit = pins()
    run("git", "add", ANSWERS)
    if run("git", "status", "--porcelain", ANSWERS).strip():
        run("git", "commit", "-q", "-m", f"toolchain: answers of {name} (answers.py --write on each CI host)")
    existing = open_pr()
    if not existing and run("git", "rev-parse", "HEAD").strip() == run("git", "rev-parse", f"origin/{BRANCH}").strip():
        run("git", "commit", "-q", "--allow-empty", "-m", f"toolchain: run the merge gate on {name}")
    main_pin = run("git", "show", "origin/main:harness/toolchain").strip()
    main_commit = re.search(r"(?m)^commit (\w+)", run("git", "show", f"origin/main:{os.path.relpath(PINS, ROOT)}")).group(1)
    title = f"toolchain: pin {name}"
    body = "\n".join([
        f"Moves blaze's toolchain from {main_pin} to {name} (rustc {rustc}).",
        "",
        f"- upstream changes: https://github.com/rust-lang/rust/compare/{main_commit[:12]}...{commit[:12]}",
        f"- opened by the daily bump ({RUN_URL}), `harness/toolchain_bump.py` and `answers.py --write` on "
        "Linux x86-64, Linux AArch64 and macOS arm64",
        "",
        "The merge gate runs on it as on any pull request; the release train runs on this branch, and "
        f"when no job or harness is red that is green on the train of the main commit it is on ({BASE}), it is queued with "
        "`gh pr merge --auto`. Otherwise the verdict is a comment here and the issue \"Toolchain bump to "
        "<date> is blocked\". Fixes for what the new nightly breaks go on this branch; the next daily run "
        "merges main into it and moves it to the newest nightly, keeping them (docs/toolchain.md).",
    ])
    if existing:
        gh("pr", "edit", str(existing["number"]), "-R", REPO, "--title", title, "--body", body)
        number = existing["number"]
    else:
        url = gh("pr", "create", "-R", REPO, "--base", "main", "--head", BRANCH, "--title", title, "--body", body).strip()
        number = int(url.rsplit("/", 1)[1])
    # The bot's push, after the pull request is open, is what runs the merge gate on it.
    run("git", "push", "-q", "origin", f"HEAD:refs/heads/{BRANCH}")
    output(number=number)
    gh("workflow", "run", "train.yaml", "-R", REPO, "--ref", BRANCH)
    on = run("git", "merge-base", "HEAD", "origin/main").strip()
    start_base_train(on)
    print(f"#{number}: {title}; the train is started on {BRANCH} and on main's {on[:10]} ({BASE})")


# ---------------------------------------------------------------------------------------------
# verdict


def jobs_of(run_id):
    out = gh("api", "--paginate", f"repos/{REPO}/actions/runs/{run_id}/jobs?per_page=50", "--jq", ".jobs[]")
    return [json.loads(l) for l in out.splitlines() if l.strip()]


def failed_harnesses(job_id):
    # A newer gh refuses to print a log with terminal escapes without the flag; an older one has
    # no such flag.
    path = f"repos/{REPO}/actions/jobs/{job_id}/logs"
    done = subprocess.run(["gh", "api", "--allow-escape-sequences", path], capture_output=True, text=True)
    if done.returncode and "unknown flag" in done.stderr:
        done = subprocess.run(["gh", "api", path], capture_output=True, text=True)
    log = done.stdout
    found = re.findall(r"failed: (.+)$", ANSI.sub("", log), re.M)
    if not found or found[-1].strip() == "none":
        return None
    return {h.strip() for h in found[-1].split(",") if h.strip()}


def failures(run_id):
    """{job name: set of harnesses that failed, or None for the job as a whole} of a train run."""
    red = {}
    for job in jobs_of(run_id):
        if job["conclusion"] in FAILED and job["name"] not in GATHERERS and not job["name"].startswith(NOT_BLOCKING):
            red[job["name"]] = failed_harnesses(job["id"]) if "harness" in job["name"] else None
    return red


def main_train(workflow_id):
    """main's newest train that finished (not cancelled): its id and head commit."""
    runs = json.loads(gh("api", f"repos/{REPO}/actions/workflows/{workflow_id}/runs?branch=main&status=completed&per_page=50",
                         "--jq", "[.workflow_runs[] | select(.conclusion != \"cancelled\") | {id, head_sha, html_url}]"))
    return runs[0] if runs else None


def start_base_train(sha):
    """Points BASE at `sha` and starts the train on it. GITHUB_TOKEN's ref update starts no
    workflow; its dispatch does."""
    moved = subprocess.run(["gh", "api", "-X", "PATCH", f"repos/{REPO}/git/refs/heads/{BASE}",
                            "-f", f"sha={sha}", "-F", "force=true"], capture_output=True, text=True)
    if moved.returncode:
        gh("api", "-X", "POST", f"repos/{REPO}/git/refs", "-f", f"ref=refs/heads/{BASE}", "-f", f"sha={sha}")
    gh("workflow", "run", "train.yaml", "-R", REPO, "--ref", BASE)


def base_runs(workflow_id, sha):
    return json.loads(gh("api", f"repos/{REPO}/actions/workflows/{workflow_id}/runs?branch={BASE}&head_sha={sha}&per_page=20",
                         "--jq", "[.workflow_runs[] | {id, status, conclusion, head_sha, html_url}]"))


def base_train(train, wait=datetime.timedelta(minutes=120)):
    """The finished train of the main commit the bump's train is on, started if there is none
    and waited for; main's newest finished train if it was cancelled or did not finish in time."""
    on = json.loads(gh("api", f"repos/{REPO}/compare/main...{train['head_sha']}"))["merge_base_commit"]["sha"]
    runs = base_runs(train["workflow_id"], on)
    if not any(it["conclusion"] != "cancelled" for it in runs):
        print(f"no train of main's {on[:10]}: starting one on {BASE}")
        start_base_train(on)
    deadline = datetime.datetime.now() + wait
    while True:
        runs = base_runs(train["workflow_id"], on)
        done = [it for it in runs if it["status"] == "completed" and it["conclusion"] != "cancelled"]
        if done:
            return done[0], f"the train of main's {on[:10]}, which the bump is on"
        if not any(it["status"] != "completed" for it in runs) and runs:
            break
        if datetime.datetime.now() > deadline:
            break
        print(f"waiting for the train of main's {on[:10]} ({', '.join(it['html_url'] for it in runs) or 'not listed yet'})")
        time.sleep(60)
    return main_train(train["workflow_id"]), f"main's newest finished train (main's {on[:10]} has none)"


def base_run(run_id):
    """Outputs run and sha of the train the bump's train RUN is compared with (`base_train`)."""
    base, which = base_train(json.loads(gh("api", f"repos/{REPO}/actions/runs/{run_id}")))
    if not base:
        raise Failed("no train of main to compare with")
    print(f"compared with {which}: {base['html_url']}")
    output(run=base["id"], sha=base["head_sha"])


def poke_auto_merge(pr):
    """GitHub merges an auto-merge pull request when a check completes after it was set, and the
    gate's `ci` finished hours before the train: `ci` runs again (a few seconds) so it merges."""
    runs = json.loads(gh("api", f"repos/{REPO}/commits/{pr['headRefOid']}/check-runs?check_name=ci",
                         "--jq", "[.check_runs[] | {id, status, conclusion, html_url}]"))
    done = [it for it in runs if it["status"] == "completed" and it["conclusion"] == "success"]
    found = re.search(r"/actions/runs/(\d+)/job/", done[0]["html_url"]) if done else None
    if found:
        gh("run", "rerun", found.group(1), "-R", REPO, "--job", str(done[0]["id"]))


def on_main_push():
    """The workflows a push to main starts that can be dispatched (path filters aside)."""
    found = []
    for name in sorted(os.listdir(os.path.join(ROOT, ".github/workflows"))):
        text = open(os.path.join(ROOT, ".github/workflows", name)).read()
        if re.search(r"^  push:\n    branches: \[\"?main\"?\]", text, re.M) and "\n  workflow_dispatch:" in text:
            found.append(name)
    return found


def after_merge(pr):
    """A merge that GITHUB_TOKEN's auto-merge makes starts no workflow on main (no train, no
    gate of main): once it merges, main's push workflows are dispatched on main."""
    for _ in range(30):
        state = json.loads(gh("pr", "view", str(pr["number"]), "-R", REPO, "--json", "state"))["state"]
        if state != "OPEN":
            break
        time.sleep(30)
    if state != "MERGED":
        print(f"the pull request is {state.lower()} after 15 minutes: main's next push starts its workflows")
        return
    for name in on_main_push():
        gh("workflow", "run", name, "-R", REPO, "--ref", "main", check=False)
        print(f"dispatched {name} on main")


verdict_date = datetime.date.today().isoformat()


def verdict(run_id):
    global verdict_date
    train = json.loads(gh("api", f"repos/{REPO}/actions/runs/{run_id}"))
    if train["head_branch"] != BRANCH:
        raise Failed(f"run {run_id} is a train on {train['head_branch']}, not on {BRANCH}")
    # The verdict runs main's checkout: the bump's pin is read at the train's commit.
    name = gh("api", f"repos/{REPO}/contents/harness/toolchain?ref={train['head_sha']}",
              "-H", "Accept: application/vnd.github.raw").strip()
    date = verdict_date = name.removeprefix("nightly-")
    pr = open_pr()
    if not pr:
        print(f"no open pull request from {BRANCH}; nothing to decide")
        return
    if train["conclusion"] == "cancelled":
        print("the train was cancelled: a newer one decides")
        return
    ours = failures(run_id)
    base, which = base_train(train)
    theirs = failures(base["id"]) if base else {}
    new = []
    for job, harnesses in sorted(ours.items()):
        if job not in theirs:
            new.append(f"- {job}" + (f": {', '.join(sorted(harnesses))}" if harnesses else ""))
        elif harnesses and theirs[job] is not None and (harnesses - theirs[job]):
            new.append(f"- {job}: {', '.join(sorted(harnesses - theirs[job]))} (main's train has {', '.join(sorted(theirs[job]))} red)")
    already = [f"- {job}" for job in sorted(ours) if job in theirs and not any(l.startswith(f"- {job}") for l in new)]
    lines = [f"Train on {train['head_sha'][:10]}: {train['html_url']}",
             f"Compared with {which}: {base['html_url'] if base else 'none found'}"]
    if new:
        lines += ["", "Red here and green on main (what the bump broke):", *new]
    if already:
        lines += ["", "Red here and on main too (not counted against the bump):", *already]
    body = "\n".join(lines)
    attempt = train.get("run_attempt", 1)
    # A harness that fails one run in ten (a timing, a port) would otherwise block a day's bump:
    # the red jobs run again, and the verdict of that attempt decides.
    if new and attempt < ATTEMPTS and subprocess.run(["gh", "run", "rerun", str(run_id), "-R", REPO, "--failed"]).returncode == 0:
        gh("pr", "comment", str(pr["number"]), "-R", REPO, "--body",
           f"**Red here and green on main: running the failed jobs again** (attempt {attempt + 1} "
           f"of {ATTEMPTS}), and that attempt decides.\n\n{body}")
        return
    if new:
        gh("pr", "comment", str(pr["number"]), "-R", REPO, "--body", f"**The bump is blocked.**\n\n{body}")
        blocked(date, f"The bump {pr['url']} ({name}) is red.\n\n{body}")
        sys.exit(1)
    gh("pr", "merge", str(pr["number"]), "-R", REPO, "--merge", "--auto")
    poke_auto_merge(pr)
    gh("pr", "comment", str(pr["number"]), "-R", REPO,
       "--body", f"**The bump's gates are green**: queued with `gh pr merge --auto`.\n\n{body}")
    unblocked(date, f"is green and queued ({pr['url']})")
    after_merge(pr)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = ap.add_subparsers(dest="step", required=True)
    sub.add_parser("prepare").add_argument("--date")
    sub.add_parser("answers").add_argument("files", nargs="+")
    sub.add_parser("pr")
    sub.add_parser("base-run").add_argument("run")
    sub.add_parser("verdict").add_argument("run")
    b = sub.add_parser("blocked")
    b.add_argument("why_file")
    b.add_argument("--date", default=datetime.date.today().isoformat())
    args = ap.parse_args()
    try:
        if args.step == "prepare":
            prepare(args.date)
        elif args.step == "answers":
            answers(args.files)
        elif args.step == "pr":
            pr()
        elif args.step == "base-run":
            base_run(args.run)
        elif args.step == "verdict":
            verdict(args.run)
        else:
            why = open(args.why_file).read() if os.path.exists(args.why_file) else "a step failed before it said why"
            blocked(args.date, f"The daily bump stopped before its pull request:\n\n{why}")
    except Failed as error:
        print(f"error: toolchain_bump_ci {args.step}: {error}", file=sys.stderr)
        if args.step == "prepare":
            open(os.path.join(os.environ.get("RUNNER_TEMP", "/tmp"), "bump-failed.txt"), "w").write(str(error))
        if args.step == "verdict":
            try:
                blocked(verdict_date, f"The verdict on the train {SERVER}/{REPO}/actions/runs/{args.run} "
                        f"failed before it decided:\n\n```\n{error}\n```\n\nIt runs again with "
                        f"`gh workflow run toolchain-bump.yaml -f train={args.run}`.")
            except Failed as also:
                print(f"error: and the issue could not be updated: {also}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
