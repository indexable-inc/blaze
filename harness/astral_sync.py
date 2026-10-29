#!/usr/bin/env python3
"""astral_sync.py: files every merged pull request of Astral's fork of blaze on our tracker.

    astral_sync.py ['<json>' | FILE | -]

Astral (astral-experiments/blaze, mirrored as indexable-inc/blaze-astral) branched from our
main at 6b3ed181. Each of its merged pull requests ends up in exactly one place here:

  - a sub-issue of #1304 (one change we should have, possibly several of their PRs),
  - the checklist of #1331 (a fix to their mixed rustc build, the "native" path our main
    removed in #408 and #886; checked there for whether blaze's own path has the same bug),
  - or #1304's "Not tracked" line (their CI, their docs, removals).

A PR is already filed when any of those links it (`astral-experiments/blaze#N`, its pull URL,
or `astral#N` in #1304 and #1331) or a body holds its marker `<!-- astral-pr: N -->`. Every
other merged PR is filed by `route` (below), so a second run files nothing new.

The input is one JSON object, every field optional:

  dry_run   true: say what would be filed and file nothing (default false)
  route     {"<PR number>": WHERE}: where a new PR goes, overriding the guess:
              "issue"   a new sub-issue of #1304
              "native"  a line on #1331's checklist
              "skip"    #1304's "Not tracked" line
              <number>  appended to that existing sub-issue (grouped with what it ports)
  labels    the labels of a new sub-issue (default ["enhancement", "P2"])

A new PR without a route is not filed: it is printed with its summary and the run exits 2, so
it is routed on the next run. Titles do not tell their mixed-build fixes from the rest (a guess
from their words put 17 of the first 77 in the wrong place). Needs `gh` signed in with write
access to the issues.
"""
import json, re, subprocess, sys, typing
from dataclasses import dataclass, field, fields

REPO = "indexable-inc/blaze"
ASTRAL = "astral-experiments/blaze"
PARENT = 1304
NATIVE = 1331
MARKER = re.compile(r"<!-- astral-pr: (\d+) -->")
LINKED = re.compile(r"(?:astral-experiments/blaze(?:/pull/|#)|\bastral#)(\d+)")

@dataclass(frozen=True)
class Input:
    dry_run: bool = False
    route: dict = field(default_factory=dict)
    labels: list = field(default_factory=lambda: ["enhancement", "P2"])


def typed(text):
    """The Input from the JSON `text` (inline, a path, or `-`); exits naming the field that is wrong."""
    if text == "-":
        text = sys.stdin.read()
    elif text and not text.lstrip().startswith("{"):
        text = open(text).read()
    try:
        value = json.loads(text or "{}")
    except json.JSONDecodeError as e:
        sys.exit(f"astral_sync.py: the input is not JSON: {e}")
    if not isinstance(value, dict):
        sys.exit("astral_sync.py: the input is a JSON object")
    known = {f.name for f in fields(Input)}
    if unknown := sorted(set(value) - known):
        sys.exit(f"astral_sync.py: no field {', '.join(unknown)} (fields: {', '.join(sorted(known))})")
    hints = typing.get_type_hints(Input)
    for name, given in value.items():
        if type(given) is not hints[name]:
            sys.exit(f"astral_sync.py: field {name} is a {hints[name].__name__}, not {given!r}")
    for pr, where in value.get("route", {}).items():
        if not pr.isdigit():
            sys.exit(f"astral_sync.py: field route has key {pr!r}, which is not a PR number")
        if where not in ("issue", "native", "skip") and type(where) is not int:
            sys.exit(f"astral_sync.py: field route[{pr}] is \"issue\", \"native\", \"skip\" or an issue number, not {where!r}")
    if not all(type(label) is str for label in value.get("labels", [])):
        sys.exit(f"astral_sync.py: field labels is a list of strings, not {value['labels']!r}")
    return Input(**value)


def gh(*args, stdin=None):
    ran = subprocess.run(["gh", *args], input=stdin, capture_output=True, text=True)
    if ran.returncode != 0:
        sys.exit(f"astral_sync.py: gh {' '.join(args[:3])} failed: {ran.stderr.strip()}")
    return ran.stdout


def gh_json(*args):
    return json.loads(gh(*args))


def filed(text):
    return {int(n) for n in MARKER.findall(text)} | {int(n) for n in LINKED.findall(text)}


def merged():
    """Astral's merged pull requests, oldest first."""
    prs = gh_json("pr", "list", "-R", ASTRAL, "--state", "merged", "--limit", "1000",
                  "--json", "number,title,body,url,additions,deletions,mergedAt")
    return sorted(prs, key=lambda pr: pr["number"])


def summary(body):
    """The first paragraph of a PR's summary, on one line."""
    text = re.sub(r"^#+ *Summary *\n", "", (body or "").strip(), flags=re.I)
    first = re.split(r"\n\s*\n|\n#", text, maxsplit=1)[0]
    return " ".join(first.split())


def line(pr):
    return f"[{ASTRAL}#{pr['number']}]({pr['url']}): {pr['title']} (+{pr['additions']}/-{pr['deletions']})"


def main():
    if len(sys.argv) > 2:
        sys.exit(f"usage: astral_sync.py ['<json>' | FILE | -]\n\n{__doc__}")
    given = typed(sys.argv[1] if len(sys.argv) == 2 else "")
    parent = gh_json("api", f"repos/{REPO}/issues/{PARENT}")
    subs = gh_json("api", "--paginate", "--slurp", f"repos/{REPO}/issues/{PARENT}/sub_issues")
    subs = [issue for page in subs for issue in page]
    native = gh_json("api", f"repos/{REPO}/issues/{NATIVE}")
    known = filed(parent["body"]) | filed(native["body"])
    for issue in subs:
        known |= filed(issue["body"] or "")
    by_number = {issue["number"]: issue for issue in subs}

    new = [pr for pr in merged() if pr["number"] not in known]
    print(f"{len(known)} of Astral's PRs already filed; {len(new)} new")
    skipped, checklist, unrouted = [], [], []
    for pr in new:
        n = pr["number"]
        where = given.route.get(str(n))
        if where is None:
            unrouted.append(pr)
            continue
        print(f"  astral#{n} -> {where}: {pr['title']}")
        if where == "skip":
            skipped.append(n)
        elif where == "native":
            checklist.append(f"- [ ] [astral#{n}]({pr['url']}): {pr['title']} <!-- astral-pr: {n} -->")
        elif where == "issue":
            body = (f"{summary(pr['body'])}\n\n- {line(pr)}\n\nPart of #{PARENT}.\n\n"
                    f"<!-- astral-pr: {n} -->\n")
            if given.dry_run:
                continue
            url = gh("issue", "create", "-R", REPO, "--title", pr["title"], "--body-file", "-",
                     *[arg for label in given.labels for arg in ("--label", label)], stdin=body).strip()
            number = int(url.rsplit("/", 1)[1])
            issue_id = gh_json("api", f"repos/{REPO}/issues/{number}")["id"]
            gh("api", "-X", "POST", f"repos/{REPO}/issues/{PARENT}/sub_issues", "-F", f"sub_issue_id={issue_id}")
            print(f"    filed #{number}")
        else:
            if where not in by_number:
                sys.exit(f"astral_sync.py: route[{n}] is #{where}, which is not a sub-issue of #{PARENT}")
            issue = by_number[where]
            body = (issue["body"] or "").rstrip()
            added = f"- {line(pr)}\n<!-- astral-pr: {n} -->"
            body = (body.replace(f"\n\nPart of #{PARENT}.", f"\n{added}\n\nPart of #{PARENT}.", 1)
                    if f"\n\nPart of #{PARENT}." in body else f"{body}\n\n{added}")
            if not given.dry_run:
                gh("issue", "edit", str(where), "-R", REPO, "--body-file", "-", stdin=body + "\n")
                issue["body"] = body
                print(f"    added to #{where}")
    if checklist and not given.dry_run:
        gh("issue", "edit", str(NATIVE), "-R", REPO, "--body-file", "-",
           stdin=native["body"].rstrip() + "\n" + "\n".join(checklist) + "\n")
        print(f"    {len(checklist)} added to #{NATIVE}'s checklist")
    if skipped and not given.dry_run:
        body = parent["body"]
        found = re.search(r"Not tracked \(not applicable\):([^\n]*)", body)
        added = ", ".join(f"astral#{n} <!-- astral-pr: {n} -->" for n in skipped)
        body = (body[:found.end()] + ", " + added + body[found.end():] if found
                else body.rstrip() + f"\n\nNot tracked (not applicable): {added}\n")
        gh("issue", "edit", str(PARENT), "-R", REPO, "--body-file", "-", stdin=body)
        print(f"    {len(skipped)} added to #{PARENT}'s not-tracked line")
    if given.dry_run and new:
        print("dry run: nothing filed")
    if unrouted:
        print(f"\n{len(unrouted)} new PRs need a route (\"issue\", \"native\", \"skip\" or a sub-issue of #{PARENT}):")
        for pr in unrouted:
            print(f"  astral#{pr['number']} {pr['url']} (merged {pr['mergedAt'][:10]}): {pr['title']}\n"
                  f"      {summary(pr['body'])[:400]}")
        sys.exit(2)


if __name__ == "__main__":
    main()
