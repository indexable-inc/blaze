#!/usr/bin/env python3
"""Items by content against today's keys, over real commits (docs/roadmap.md, "Items by content").

    items.py compare A.jsonl B.jsonl
    items.py replay <repo> <commit>... --ra-jit <bin> --work <dir> [--json <out>]

`ra-jit items <checkout>` writes each item of a checkout with its hashes and the name today's
daemon knows it by. For a commit and its parent, this counts, over the items of the crates the
commit touches and of the crates of the checkout that depend on them, how many are unchanged:

- by `deep`, `code` and `interface` hash: found among the parent's hashes, wherever and under
  whatever name they were;
- by today's in-daemon name (file and place in the file's table of items): the patch base finds
  the old code under the same name, and compares its mark;
- by today's store key, which also names each crate by a hash of its sources and of those of the
  crates of the checkout it depends on: in the crates counted here, every key is new.

Each dump is kept in `--work` by commit, so a commit's parent is loaded once for every commit
that has it.
"""

import argparse
import json
import os
import subprocess
import sys
import time
from collections import Counter


def load(path):
    crates, items = {}, []
    with open(path) as f:
        for line in f:
            it = json.loads(line)
            if "root" in it:
                crates[it["crate"]] = it
            else:
                items.append(it)
    return crates, items


def crate_dir(root):
    """The directory of a crate from its root file: `src/lib.rs` is in it, `build.rs`'s too."""
    head = os.path.dirname(root)
    return os.path.dirname(head) if os.path.basename(head) in ("src", "examples", "benches", "tests") else head


def affected(crates, changed):
    """The crates a change to `changed` (paths below the checkout, `~/...`) names anew today: those
    a changed file is in, and the crates of the checkout that depend on them."""
    dirs = {name: crate_dir(c["root"]) for name, c in crates.items()}
    touched = set()
    for name, d in dirs.items():
        prefix = d.rstrip("/") + "/"
        if any(path.startswith(prefix) for path in changed):
            touched.add(name)
    # A file under two crates' directories is the nearest one's; a workspace root crate's
    # directory holds the others, so it is touched only by what no other crate holds.
    for name in list(touched):
        prefix = dirs[name].rstrip("/") + "/"
        inner = [p for p in changed if p.startswith(prefix)]
        nested = [o for o in dirs if o != name and dirs[o].startswith(prefix) and dirs[o] != dirs[name]]
        if inner and all(any(p.startswith(dirs[o].rstrip("/") + "/") for o in nested) for p in inner):
            touched.discard(name)
    out = set(touched)
    grew = True
    while grew:
        grew = False
        for name, c in crates.items():
            if name not in out and any(d in out for d in c["deps"]):
                out.add(name)
                grew = True
    return touched, out


def compare(before, after, changed=None):
    (crates_a, a), (crates_b, b) = before, after
    if changed is None:
        scope = set(crates_b)
        touched = scope
    else:
        touched, scope = affected(crates_b, changed)
    deep = Counter(it["deep"] for it in a)
    code = Counter(it["code"] for it in a if it["code"])
    interface = Counter(it["interface"] for it in a)
    names = {it["name"] for it in a}
    counted = [it for it in b if it["crate"] in scope]
    with_code = [it for it in counted if it["code"]]
    n = len(counted)
    share = lambda k, of: (k / of) if of else 1.0
    kinds = Counter()
    for it in counted:
        if it["deep"] not in deep:
            kinds[it["kind"]] += 1
    result = {
        "items": n,
        "crates": sorted(scope),
        "touched": sorted(touched),
        "deep": share(sum(it["deep"] in deep for it in counted), n),
        "code": share(sum(it["code"] in code for it in with_code), len(with_code)),
        "interface": share(sum(it["interface"] in interface for it in counted), n),
        "name": share(sum(it["name"] in names for it in counted), n),
        "store_key": 0.0 if n else 1.0,
        "changed_deep": n - sum(it["deep"] in deep for it in counted),
        "changed_code": len(with_code) - sum(it["code"] in code for it in with_code),
        "changed_by_kind": dict(kinds),
        "missed": sum(1 for it in counted if it["missed"]),
    }
    return result


def show(label, r):
    print(
        f"{label}: {r['items']} items in {len(r['crates'])} crates ({len(r['touched'])} touched); "
        f"unchanged by deep {r['deep']:.1%}, code {r['code']:.1%}, interface {r['interface']:.1%}; "
        f"today: name {r['name']:.1%}, store key {r['store_key']:.1%}; "
        f"new deep {r['changed_deep']}, new code {r['changed_code']}, missed {r['missed']}"
    )


def dump(ra_jit, repo, commit, work):
    out = os.path.join(work, f"{commit}.jsonl")
    if os.path.exists(out):
        return out, None
    tree = os.path.join(work, "tree")
    if not os.path.exists(tree):
        subprocess.run(["git", "-C", repo, "worktree", "add", "--detach", tree, commit], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    subprocess.run(["git", "-C", tree, "checkout", "--quiet", "--detach", commit], check=True)
    start = time.monotonic()
    done = subprocess.run([ra_jit, "items", tree, "--out", out + ".part"], capture_output=True, text=True)
    if done.returncode != 0:
        sys.stderr.write(done.stderr[-4000:])
        raise SystemExit(f"ra-jit items failed on {commit}")
    os.rename(out + ".part", out)
    summary = [line for line in done.stderr.splitlines() if line.startswith("items:")]
    return out, (time.monotonic() - start, summary[-1] if summary else "")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("compare")
    c.add_argument("before")
    c.add_argument("after")
    r = sub.add_parser("replay")
    r.add_argument("repo")
    r.add_argument("commits", nargs="+")
    r.add_argument("--ra-jit", required=True)
    r.add_argument("--work", required=True)
    r.add_argument("--json")
    args = parser.parse_args()
    if args.cmd == "compare":
        show("compare", compare(load(args.before), load(args.after)))
        return
    os.makedirs(args.work, exist_ok=True)
    results = []
    for commit in args.commits:
        sha = subprocess.run(["git", "-C", args.repo, "rev-parse", commit], capture_output=True, text=True,
                             check=True).stdout.strip()
        parent = subprocess.run(["git", "-C", args.repo, "rev-parse", f"{sha}^"], capture_output=True, text=True,
                                check=True).stdout.strip()
        subject = subprocess.run(["git", "-C", args.repo, "log", "-1", "--format=%s", sha], capture_output=True,
                                 text=True, check=True).stdout.strip()
        changed = subprocess.run(["git", "-C", args.repo, "diff", "--name-only", parent, sha], capture_output=True,
                                 text=True, check=True).stdout.split()
        changed = ["~/" + path for path in changed]
        before, spent_a = dump(args.ra_jit, args.repo, parent, args.work)
        after, spent_b = dump(args.ra_jit, args.repo, sha, args.work)
        for spent in (spent_a, spent_b):
            if spent:
                print(f"  dumped in {spent[0]:.1f} s; {spent[1]}")
        result = compare(load(before), load(after), changed)
        result.update(commit=sha[:10], subject=subject, files=len(changed))
        show(f"{sha[:10]} {subject[:60]!r} ({len(changed)} files)", result)
        results.append(result)
    if args.json:
        with open(args.json, "w") as f:
            json.dump(results, f, indent=1)


if __name__ == "__main__":
    main()
