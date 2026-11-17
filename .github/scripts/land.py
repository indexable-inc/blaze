#!/usr/bin/env python3
"""land.py: lands the current branch on main, and waits for it.

    python3 .github/scripts/land.py '{"what": "what the branch does", "smoke": "smoke.txt"}'
    python3 .github/scripts/land.py '{"follow": true}'      (or {"follow": N}: pull request N)

The input is one JSON object (inline, the path of a .json file, or `-` for standard input),
unknown fields refused (rule scripts-take-typed-input):

    what    what the branch does; the pull request is "Merge <branch>: <what>"
    smoke   the file holding the smoke quote (its first line `blaze-smoke 1`) that
            scripts/src/bin/smoke.rs printed for a release blaze built from this branch's head
    wait    follow the waiting until the merge (true); false leaves it running in the background
    follow  instead of landing: follow the waiting of this branch's pull request (true) or of
            pull request N, starting it again if it is gone

The two older shapes are read as these: `land.py "what" [--no-wait]` is {"what": ..., "wait":
...} (refused, as it names no smoke quote), and `land.py --wait [N]` is {"follow": N or true}.

Run it in the lane's worktree, on its branch, with everything committed and the local gate
passed. Nothing merges without the smoke quote of its exact tree (rule proof-before-merge): the
branch must contain origin/main (`git merge origin/main` first), so the merge commit's tree is the
head's; the quote must name that tree, clean, with verdict ok (smoke.rs decides, land.py only asks
it: `smoke '{"verify": ...}'`). It pushes the branch, opens its pull request (titled "Merge
<branch>: <what it does>", which with " (#N)" is the merge commit's subject) or takes the one
already open, and puts the quote in its body. The pull request's run is the merge gate, the build
and the unit tests on Linux AArch64 (on ix runners). Once it passes, the waiting asks smoke.rs
again about the body's quote and the head GitHub has, checks that main has not moved past the
head, and merges with a merge commit, as `git merge --no-ff` would, pinned to that head. Every
harness runs after the merge, in the release train (train.yaml). land.py waits until then and
exits 0 with the commit main is at, or exits 1 and says why the pull request did not merge (a
check that failed, a conflict with main, main moved, the quote). GitHub deletes the branch on
origin once it is merged. There is no flag around the smoke, and an admin merge is not an
exception to it (rule 68).

The waiting is done by a process of its own, detached from the terminal and from whatever ran
land.py, which writes what it sees to <git common dir>/land/pr-N.log; land.py follows that log.
So land.py killed, or run in the background by a tool that ends its process group, leaves the
waiting going. `{"follow": true}` (this branch's pull request) or `{"follow": N}` follows it
again, and starts it again if it is gone; running land.py again on the same branch does too.
"""
import dataclasses, json, os, signal, subprocess, sys, time

REPO = "indexable-inc/blaze"
POLL = 20
GIVE_UP = 3 * 3600
# A `gh` call that fails this many times in a row (the network, GitHub) ends the waiting.
TRIES = 15
EXIT = "land.py exit "
# How the quote is checked: smoke.rs's own `verify`, on the pinned release blaze, so the quote's
# format is read in one place only.
SMOKE = ["blaze", "run", "-q", "-p", "scripts", "--bin", "smoke", "--"]
# The ceiling of one `smoke` verify, in seconds: a build of the scripts crate by the pinned
# release when it is not built yet (minutes), then a check that reads one text in milliseconds.
# Past it something is wrong, and the merge waits for it.
VERIFY_TIMEOUT = 900
# What makes the quote, said in every refusal.
SMOKE_RUN = ("blaze run -q -p scripts --bin smoke -- "
             "'{\"blaze\": \"<a release blaze built from this head>\", \"work\": \"<an empty directory>\"}' > smoke.txt")
# The pull request body's smoke section, which land.py replaces whole on each run.
QUOTE_START, QUOTE_END = "<!-- smoke quote (land.py writes this) -->", "<!-- end of the smoke quote -->"


@dataclasses.dataclass(frozen=True)
class Input:
    """land.py's one input; see the module's docstring."""
    what: str = ""
    smoke: str = ""
    wait: bool = True
    follow: bool | int = False


def parse_input(argv):
    """The one JSON object of `argv`, or one of the two older shapes translated to it."""
    if argv[:1] == ["--wait"] and (len(argv) == 1 or (len(argv) == 2 and argv[1].isdigit())):
        value = {"follow": int(argv[1]) if len(argv) == 2 else True}
    elif len(argv) == 1 and (argv[0] == "-" or argv[0].lstrip().startswith("{") or argv[0].endswith(".json")):
        try:
            text = (sys.stdin.read() if argv[0] == "-" else argv[0] if argv[0].lstrip().startswith("{")
                    else open(argv[0]).read())
            value = json.loads(text)
        except (OSError, json.JSONDecodeError) as e:
            sys.exit(f"land.py: the input: {e}")
    elif argv[:1] and not argv[0].startswith("-") and argv[1:] in ([], ["--no-wait"]):
        value = {"what": argv[0], "wait": argv[1:] != ["--no-wait"]}
    else:
        sys.exit(__doc__)
    types = {"what": str, "smoke": str, "wait": bool, "follow": (bool, int)}
    if not isinstance(value, dict) or set(value) - set(types):
        sys.exit(f"land.py: the input is one JSON object of {', '.join(types)}, not {json.dumps(value)[:200]}")
    for name, given in value.items():
        if not isinstance(given, types[name]):
            sys.exit(f"land.py: {name} is not {json.dumps(given)} (see land.py's docstring)")
    asked = Input(**value)
    if bool(asked.what) == (asked.follow is not False):
        sys.exit("land.py: give `what` (to land) or `follow` (to follow a waiting), one of the two")
    return asked


def run(*args, check=True):
    done = subprocess.run(args, capture_output=True, text=True)
    if check and done.returncode:
        sys.exit(f"land.py: {' '.join(args)} failed:\n{done.stderr.strip() or done.stdout.strip()}")
    return done.stdout.strip()


class GhFailed(Exception):
    pass


def gh(*args):
    done = subprocess.run(["gh", *args], capture_output=True, text=True)
    if done.returncode:
        raise GhFailed(done.stderr.strip() or done.stdout.strip())
    return done.stdout.strip()


def state(number):
    query = """query($number: Int!) { repository(owner: "indexable-inc", name: "blaze") {
      main: ref(qualifiedName: "refs/heads/main") { target { oid } }
      pullRequest(number: $number) { state mergeable url headRefName body
        autoMergeRequest { enabledAt } mergeCommit { oid }
        commits(last: 1) { nodes { commit { oid tree { oid } } } } } } }"""
    out = json.loads(gh("api", "graphql", "-f", f"query={query}", "-F", f"number={number}"))["data"]["repository"]
    pr = out["pullRequest"]
    head = pr["commits"]["nodes"][0]["commit"]
    return {**pr, "head": head["oid"], "head_tree": head["tree"]["oid"], "main": out["main"]["target"]["oid"]}


def required_checks(number):
    # `gh pr checks` exits non-zero while checks fail or are pending; its output is still the list.
    out = run("gh", "pr", "checks", str(number), "--repo", REPO, "--required", "--json", "name,bucket,link", check=False)
    try:
        return json.loads(out or "[]")
    except json.JSONDecodeError:
        return []


def refusal(why):
    """The message of a merge refused for its smoke quote: why, the rule, and what to run."""
    return (f"land.py: refused: {why}\n"
            "  A merge carries the smoke quote of a release blaze built from its exact tree (rule proof-before-merge);\n"
            "  admin merges need the smoke quote too: rule 68. No flag skips it.\n"
            f"  Run, on blaze-vm (how-to H-smoke-before-merge): {SMOKE_RUN}\n"
            "  then: python3 .github/scripts/land.py '{\"what\": \"...\", \"smoke\": \"smoke.txt\"}'")


def quote_refusal(quote, tree):
    """None when `quote` holds a smoke quote of `tree` whose verdict is ok, else why not, as
    smoke.rs says it (this never reads the quote's format itself)."""
    asked = json.dumps({"verify": {"quote": quote, "tree": tree}})
    try:
        done = subprocess.run([*SMOKE, asked], capture_output=True, text=True, timeout=VERIFY_TIMEOUT)
    except subprocess.TimeoutExpired:
        return f"`smoke` did not answer within {VERIFY_TIMEOUT} s"
    except OSError as e:
        return f"`smoke` could not run ({' '.join(SMOKE)}): {e}"
    said = done.stdout.strip().splitlines()
    if done.returncode == 0 and said[-1:] == ["ok"]:
        return None
    return said[-1] if done.returncode == 1 and said else (
        f"`smoke` exited {done.returncode}: {(done.stderr.strip() or done.stdout.strip())[-2000:]}")


def with_quote(body, quote):
    """`body` with its smoke section replaced by `quote`, or the section appended."""
    section = f"{QUOTE_START}\n```\n{quote.strip()}\n```\n{QUOTE_END}"
    if QUOTE_START in body and QUOTE_END in body.split(QUOTE_START, 1)[1]:
        before, rest = body.split(QUOTE_START, 1)
        return before + section + rest.split(QUOTE_END, 1)[1]
    return f"{body.rstrip()}\n\n{section}\n"


def files(number):
    """The log and pid file of the waiting for pull request `number`, in the git common
    directory, which outlives the lane's worktree."""
    common = os.path.abspath(run("git", "rev-parse", "--git-common-dir"))
    directory = os.path.join(common, "land")
    os.makedirs(directory, exist_ok=True)
    return os.path.join(directory, f"pr-{number}.log"), os.path.join(directory, f"pr-{number}.pid")


def alive(pid_file):
    try:
        pid = int(open(pid_file).read().strip())
        os.kill(pid, 0)
        return pid
    except (OSError, ValueError):
        return None


def wait(number):
    """The waiting itself, in the detached process: one line to the log for each change, the
    merge once the checks pass and the quote holds, then `land.py exit N`."""
    def say(line):
        print(f"{time.strftime('%H:%M:%S')} {line}", flush=True)

    def end(code, line):
        say(line)
        print(f"{EXIT}{code}", flush=True)
        sys.exit(code)

    said, asked, failures, start = None, 0, 0, time.monotonic()
    while time.monotonic() - start < GIVE_UP:
        try:
            pr = state(number)
            failures = 0
        except GhFailed as e:
            failures += 1
            if failures >= TRIES:
                end(1, f"land.py: gh failed {TRIES} times in a row, last: {e}")
            time.sleep(POLL)
            continue
        url, branch = pr["url"], pr["headRefName"]
        if pr["state"] == "MERGED":
            end(0, f"merged: main is at {pr['mergeCommit']['oid'][:10]} or after it")
        if pr["state"] == "CLOSED":
            end(1, f"land.py: {url} was closed")
        if asked:
            # The merge was asked for and answered; it shows as MERGED within a poll or two.
            asked += 1
            if asked > 3:
                end(1, f"land.py: {url} was merged by `gh pr merge` and is still open; see it")
            time.sleep(POLL)
            continue
        if pr["mergeable"] == "CONFLICTING":
            end(1, f"land.py: {branch} conflicts with main: merge origin/main into it, smoke it again, then run land.py again")
        checks = required_checks(number)
        failed = [c for c in checks if c["bucket"] == "fail"]
        if failed:
            end(1, "land.py: the pull request's checks failed, so it did not merge:\n"
                + "\n".join(f"  {c['name']}: {c['link']}" for c in failed))
        if checks and all(c["bucket"] in ("pass", "skipping") for c in checks):
            # The quote names the head's tree, and so does the merge commit: a head that
            # contains main merged with --no-ff takes nothing from main it lacks, so the merge's
            # tree is the head's. If main has moved past the head since the smoke, the merge's
            # tree would be one nothing ran on: refused, and the quote is made again after
            # `git merge origin/main`, as its tree changed. Main can still move in the seconds
            # between this comparison and the merge; the merge is pinned to the head checked.
            try:
                behind = int(gh("api", f"repos/{REPO}/compare/{pr['main']}...{pr['head']}", "--jq", ".behind_by"))
            except (GhFailed, ValueError) as e:
                end(1, f"land.py: could not compare {branch} with main: {e}")
            if behind:
                end(1, refusal(f"main moved {behind} commit(s) past {branch}'s head after its smoke, so the merge's "
                               "tree is not the quote's: git merge origin/main, build a release blaze of it, run the "
                               "smoke on it, then land.py again"))
            why = quote_refusal(pr["body"] or "", pr["head_tree"])
            if why:
                end(1, refusal(f"the pull request's smoke quote, for head {pr['head'][:10]}: {why}"))
            merged = subprocess.run(["gh", "pr", "merge", str(number), "--repo", REPO, "--merge",
                                     "--match-head-commit", pr["head"]], capture_output=True, text=True)
            if merged.returncode:
                end(1, f"land.py: gh pr merge failed:\n{merged.stderr.strip()}")
            say(f"checks passed and the smoke quote holds for {pr['head'][:10]}: merged")
            asked = 1
            continue
        now = "waiting for the pull request's checks"
        if now != said:
            say(now)
            said = now
        time.sleep(POLL)
    end(1, f"land.py: gave up waiting after {GIVE_UP // 3600} hours; pull request #{number} did not merge")


def detach(number, log, pid_file):
    """Starts the waiting for `number` in a process of its own session, its parent gone, so
    neither a closed terminal nor the end of the caller's process group or tree ends it."""
    script = os.path.abspath(__file__)
    if not hasattr(os, "fork"):
        flags = subprocess.CREATE_NEW_PROCESS_GROUP | subprocess.DETACHED_PROCESS
        with open(log, "a") as out:
            child = subprocess.Popen([sys.executable, script, "--waiter", str(number)], stdin=subprocess.DEVNULL,
                                     stdout=out, stderr=subprocess.STDOUT, creationflags=flags)
        open(pid_file, "w").write(str(child.pid))
        return
    sys.stdout.flush()
    if os.fork():
        # The pid file is written once the first child has exited.
        os.wait()
        return
    os.setsid()
    grandchild = os.fork()
    if grandchild:
        open(pid_file, "w").write(str(grandchild))
        os._exit(0)
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
    fd = os.open(log, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
    null = os.open(os.devnull, os.O_RDONLY)
    os.dup2(null, 0)
    os.dup2(fd, 1)
    os.dup2(fd, 2)
    os.execv(sys.executable, [sys.executable, script, "--waiter", str(number)])


def start(number):
    """Starts the waiting for `number` unless a process is waiting already: its log and pid file."""
    log, pid_file = files(number)
    if not alive(pid_file):
        if os.path.exists(log) and EXIT in open(log).read():
            # A waiting that ended before: its outcome stands unless the pull request moved on.
            os.rename(log, log + ".old")
        detach(number, log, pid_file)
    return log, pid_file


def follow(number):
    """Follows the waiting for `number`, starting it if no process is waiting, until it ends;
    exits as it does."""
    log, pid_file = start(number)
    print(f"land.py: waiting in the background; log {log} (land.py '{{\"follow\": {number}}}' follows it again)", flush=True)
    position, idle = 0, 0
    while True:
        text = open(log).read() if os.path.exists(log) else ""
        new, position = text[position:], len(text)
        for line in new.splitlines():
            if line.startswith(EXIT):
                sys.exit(int(line[len(EXIT):]))
            print(line, flush=True)
        if not new and not alive(pid_file):
            idle += 1
            if idle >= 3:
                sys.exit(f"land.py: the waiting process ended without an outcome; see {log}, then land.py '{{\"follow\": {number}}}'")
        else:
            idle = 0
        time.sleep(1)


def main():
    if sys.argv[1:2] == ["--waiter"]:
        return wait(int(sys.argv[2]))
    asked = parse_input(sys.argv[1:])
    if asked.follow is not False:
        if asked.follow is not True:
            return follow(asked.follow)
        branch = run("git", "rev-parse", "--abbrev-ref", "HEAD")
        found = json.loads(run("gh", "pr", "list", "--repo", REPO, "--head", branch, "--state", "all",
                               "--limit", "1", "--json", "number"))
        if not found:
            sys.exit(f"land.py: {branch} has no pull request")
        return follow(found[0]["number"])
    branch = run("git", "rev-parse", "--abbrev-ref", "HEAD")
    if branch in ("HEAD", "main"):
        sys.exit("land.py: check out the lane's branch first; main moves only by merged pull requests")
    if run("git", "status", "--porcelain", "--untracked-files=no"):
        sys.exit("land.py: commit or drop the changes in the worktree first; only what is committed lands")
    run("git", "fetch", "-q", "origin", "main")
    if not run("git", "rev-list", "origin/main..HEAD"):
        sys.exit(f"land.py: {branch} has nothing that main does not have")
    if subprocess.run(["git", "merge-base", "--is-ancestor", "origin/main", "HEAD"]).returncode:
        sys.exit(refusal(f"{branch} does not contain origin/main, so the merge's tree would be one nothing ran on: "
                         "git merge origin/main, build a release blaze of the result, run the smoke on it, then land.py again"))
    if not asked.smoke:
        sys.exit(refusal("no `smoke` file was given"))
    try:
        quote = open(asked.smoke).read()
    except OSError as e:
        sys.exit(refusal(f"the smoke file: {e}"))
    why = quote_refusal(quote, run("git", "rev-parse", "HEAD^{tree}"))
    if why:
        sys.exit(refusal(f"{asked.smoke}: {why}"))

    pushed = subprocess.run(["git", "push", "-q", "-u", "origin", f"HEAD:refs/heads/{branch}"], capture_output=True, text=True)
    if pushed.returncode:
        sys.exit(f"land.py: origin's {branch} has commits this one does not (git pull, or push it yourself):\n{pushed.stderr.strip()}")

    found = json.loads(run("gh", "pr", "list", "--repo", REPO, "--head", branch, "--state", "open", "--json", "number,url,body,autoMergeRequest"))
    if found:
        number, url = found[0]["number"], found[0]["url"]
        run("gh", "pr", "edit", str(number), "--repo", REPO, "--body", with_quote(found[0]["body"] or "", quote))
        if found[0]["autoMergeRequest"]:
            # Set by an older land.py: GitHub would merge without the waiting's checks of the quote.
            run("gh", "pr", "merge", str(number), "--repo", REPO, "--disable-auto")
    else:
        body = with_quote(run("git", "log", "--no-merges", "--format=- %s", "origin/main..HEAD"), quote)
        url = run("gh", "pr", "create", "--repo", REPO, "--base", "main", "--head", branch,
                  "--title", f"Merge {branch}: {asked.what}", "--body", body)
        number = int(url.rstrip("/").rsplit("/", 1)[1])
    print(f"{url}")
    if not asked.wait:
        log, _ = start(number)
        print(f"land.py: the waiting merges it once its checks pass; log {log} (land.py '{{\"follow\": {number}}}' follows it)")
        return
    follow(number)


if __name__ == "__main__":
    main()
