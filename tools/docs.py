#!/usr/bin/env python3
"""docs.py: every document under docs/ says what it is, and the current ones are held to the tree.

    python3 tools/docs.py check    fail naming each problem (CI, beside tools/agents/rules.ts)
    python3 tools/docs.py write    regenerate docs/README.md, the index, from the front matter

Each docs/**/*.md but docs/README.md (generated) and docs/upstream/ (others' text, as fetched)
opens with front matter, one `key: value` per line between `---` lines:

    status: current | plan | record | superseded
    summary: one line, for the index
    date: YYYY-MM-DD        a plan's, a record's or a superseded doc's: when it was written
    superseded_by: path#anchor, relative to the doc as a link is, for a superseded doc alone

`current` says how blaze is now and is kept true: its links and anchors resolve, the repository
paths it names in backticks exist, and the BLAZE_* settings it names are in the code. A `plan` is a
design not (fully) built, dated: its links and anchors resolve, but it may name paths still to come.
A `record` (research, a report, a measurement) was true on its date and is never brought up to
date: only the files it links have to exist. A `superseded` doc names what replaced it and is left
out of the index. Outside docs/, every doc under docs/ that code or another document names exists.
"""
import os, re, subprocess, sys, unicodedata
from dataclasses import dataclass
from typing import Literal, get_args
from urllib.parse import unquote

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DOCS = os.path.join(ROOT, "docs")
INDEX = os.path.join(DOCS, "README.md")
Status = Literal["current", "plan", "record", "superseded"]
# Top-level directories whose paths a doc names in backticks; anything else in backticks is a
# command, a type or a relative path, not checked.
TREE = ("crates", "harness", "tools", "docs", "site", "library", "vendor", ".github", "demo", "licenses")
PATH = re.compile(rf"(?:{'|'.join(re.escape(t) for t in TREE)})/[\w./@+-]*")
SETTING = re.compile(r"\bBLAZE_[A-Z0-9_]*[A-Z0-9](?![\w*])")
DOC_NAMED = re.compile(r"(?<![\w./-])docs/[\w./-]+\.md\b")
LINK = re.compile(r"!?\[(?:[^\[\]]|\[[^\]]*\])*\]\(\s*<?([^)\s>]+)>?(?:\s+\"[^\"]*\")?\s*\)")
REF_LINK = re.compile(r"^\s{0,3}\[[^\]]+\]:\s*<?(\S+?)>?(?:\s|$)", re.M)
SPAN = re.compile(r"(`+)(.+?)\1")
HTML_ID = re.compile(r"<a\s+(?:name|id)=\"([^\"]+)\"")
# Not checked for the docs they name: docs/ itself (its links are), the history, others' code
# (the standard library's own docs/, vendored crates) and skills about other projects.
NOT_OURS = ("docs/", "CHANGELOG.md", "library/", "vendor/", ".agents/")


@dataclass(frozen=True)
class Front:
    status: Status
    summary: str
    date: str | None = None
    superseded_by: str | None = None


@dataclass(frozen=True)
class Doc:
    path: str  # relative to ROOT
    front: Front | None
    body: str  # without front matter or fenced code


def rel(path):
    return os.path.relpath(path, ROOT)


def strip_fences(text):
    out, fence = [], None
    for line in text.split("\n"):
        m = re.match(r"\s{0,3}(`{3,}|~{3,})", line)
        if fence is None and m:
            fence = m.group(1)
            out.append("")
        elif fence is not None:
            if line.strip().startswith(fence[0] * len(fence)) and line.strip().strip(fence[0]) == "":
                fence = None
            out.append("")
        else:
            out.append(line)
    return "\n".join(out)


def slug(heading, seen):
    text = LINK.sub(lambda m: m.group(0)[1:m.group(0).index("](")], heading)
    text = "".join(c for c in text.strip().lower()
                   if c.isalnum() or c in "-_ " or unicodedata.category(c).startswith("M"))
    text = text.replace(" ", "-")
    n = seen.get(text, 0)
    seen[text] = n + 1
    return text if n == 0 else f"{text}-{n}"


def anchors(body):
    seen = {}
    out = {slug(m.group(1), seen) for m in re.finditer(r"^\s{0,3}#{1,6}\s+(.+?)\s*#*\s*$", body, re.M)}
    return out | set(HTML_ID.findall(body))


def parse_front(path, text, errors):
    if not text.startswith("---\n"):
        errors.append(f"{path}: no front matter (`---`, then status, summary and the rest; tools/docs.py)")
        return None, text
    end = text.find("\n---\n", 4)
    if end < 0:
        errors.append(f"{path}: front matter has no closing `---`")
        return None, text
    fields = {}
    for n, line in enumerate(text[4:end].split("\n"), 2):
        key, sep, value = line.partition(": ")
        if not sep or not value.strip():
            errors.append(f"{path}:{n}: front matter line is not `key: value`: {line!r}")
            continue
        if key in fields:
            errors.append(f"{path}:{n}: `{key}` given twice")
        fields[key] = value.strip()
    body = text[end + 5:]
    unknown = set(fields) - set(Front.__dataclass_fields__)
    if unknown:
        errors.append(f"{path}: unknown front matter {', '.join(sorted(unknown))} (status, summary, date, superseded_by)")
        return None, body
    if "status" not in fields or "summary" not in fields:
        errors.append(f"{path}: front matter needs `status` and `summary`")
        return None, body
    front = Front(**fields)
    if front.status not in get_args(Status):
        errors.append(f"{path}: status `{front.status}` is not one of {', '.join(get_args(Status))}")
    if front.status != "current" and front.date is None:
        errors.append(f"{path}: a {front.status} doc needs `date: YYYY-MM-DD`")
    if front.date is not None and not re.fullmatch(r"\d{4}-\d{2}-\d{2}", front.date):
        errors.append(f"{path}: date `{front.date}` is not YYYY-MM-DD")
    if (front.status == "superseded") != (front.superseded_by is not None):
        errors.append(f"{path}: `superseded_by` goes with status superseded, and only with it")
    return front, body


def load():
    errors, docs = [], {}
    for dirpath, dirnames, filenames in os.walk(DOCS):
        dirnames[:] = sorted(d for d in dirnames if os.path.join(dirpath, d) != os.path.join(DOCS, "upstream"))
        for name in sorted(filenames):
            full = os.path.join(dirpath, name)
            if not name.endswith(".md") or full == INDEX:
                continue
            text = open(full, encoding="utf-8").read()
            front, body = parse_front(rel(full), text, errors)
            body = strip_fences(body)
            docs[rel(full)] = Doc(rel(full), front, body)
    return docs, errors


def links(body):
    return [m.group(1) for m in LINK.finditer(body)] + REF_LINK.findall(body)


def check_link(doc, target, anchored, cache, errors):
    if re.match(r"[a-z][a-z0-9+.-]*:", target) or target.startswith("//"):
        return
    path, _, anchor = target.partition("#")
    path = unquote(path.split("?")[0])
    full = os.path.normpath(os.path.join(ROOT, os.path.dirname(doc.path), path)) if path else os.path.join(ROOT, doc.path)
    if not os.path.exists(full):
        errors.append(f"{doc.path}: links to {target}, which does not exist")
        return
    if anchored and anchor and full.endswith(".md"):
        if full not in cache:
            cache[full] = anchors(strip_fences(open(full, encoding="utf-8").read()))
        if unquote(anchor).lower() not in cache[full]:
            errors.append(f"{doc.path}: links to {target}, and {rel(full)} has no heading #{anchor}")


def tracked():
    out = subprocess.run(["git", "ls-files", "-z"], cwd=ROOT, check=True, capture_output=True).stdout
    return [p for p in out.decode().split("\0") if p]


def read(path):
    try:
        return open(os.path.join(ROOT, path), encoding="utf-8").read()
    except (UnicodeDecodeError, FileNotFoundError, IsADirectoryError):
        return None


def check():
    docs, errors = load()
    settings = set()
    for path in tracked():
        if path.startswith(NOT_OURS):
            continue
        text = read(path)
        if text is None:
            continue
        if not path.endswith(".md"):
            settings.update(SETTING.findall(text))
        if path.endswith(".md"):
            text = strip_fences(text)
        for n, line in enumerate(text.split("\n"), 1):
            for named in DOC_NAMED.findall(line):
                if not os.path.exists(os.path.join(ROOT, named)):
                    errors.append(f"{path}:{n}: names {named}, which does not exist")
    cache = {}
    for doc in docs.values():
        if doc.front is None:
            continue
        status = doc.front.status
        if doc.front.superseded_by is not None:
            check_link(doc, doc.front.superseded_by, True, cache, errors)
        for target in links(doc.body):
            check_link(doc, target, status in ("current", "plan"), cache, errors)
        if status != "current":
            continue
        for m in SPAN.finditer(doc.body):
            token = re.sub(r":\d+(-\d+)?$", "", m.group(2).strip())
            if PATH.fullmatch(token) and "..." not in token and not os.path.exists(os.path.join(ROOT, token.rstrip("/"))):
                errors.append(f"{doc.path}: names `{token}`, which does not exist")
        for name in sorted(set(SETTING.findall(re.sub(r"\[[^\]]*\]\([^)]*\)", "", doc.body)))):
            if name not in settings:
                errors.append(f"{doc.path}: names {name}, which no code reads")
    if not errors and read(rel(INDEX)) != index(docs):
        errors.append("docs/README.md is stale: run `python3 tools/docs.py write` and commit it")
    return errors


def index(docs):
    def line(doc):
        when = f" ({doc.front.date})" if doc.front.status != "current" else ""
        return f"- [{os.path.relpath(doc.path, 'docs')}]({os.path.relpath(doc.path, 'docs')}){when}: {doc.front.summary}"

    def group(status, title, intro):
        chosen = [d for d in docs.values() if d.front is not None and d.front.status == status]
        if status != "current":
            chosen.sort(key=lambda d: (d.front.date, d.path), reverse=True)
        return [f"## {title}", "", intro, ""] + [line(d) for d in chosen] + [""]

    out = ["<!-- Generated by `python3 tools/docs.py write` from each doc's front matter; edit those, not this. -->",
           "# Documents", "",
           "Each document under docs/ says in its front matter what it is (tools/docs.py). Superseded ones",
           "are left out here; each names what replaced it. docs/upstream/ holds others' text, as fetched.", ""]
    out += group("current", "Current", "How blaze is now. Kept true: CI checks their links, paths and settings against the tree.")
    out += group("plan", "Plans", "Designs not (fully) built, newest first.")
    out += group("record", "Records", "Research, reports and measurements, true on their date and not brought up to date. Newest first.")
    return "\n".join(out).rstrip("\n") + "\n"


def main():
    if len(sys.argv) != 2 or sys.argv[1] not in ("check", "write"):
        sys.exit("tools/docs.py: one subcommand, check or write")
    if sys.argv[1] == "write":
        docs, errors = load()
        if errors:
            sys.exit("\n".join(errors))
        open(INDEX, "w", encoding="utf-8").write(index(docs))
        return
    errors = check()
    if errors:
        print("\n".join(errors), file=sys.stderr)
        sys.exit(f"tools/docs.py: {len(errors)} problems")


if __name__ == "__main__":
    main()
