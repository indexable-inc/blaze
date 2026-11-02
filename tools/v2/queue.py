"""The lanes' request queue: one request per file under `v2/queue/`, each with an owner.

A request is `v2/queue/<id>.json`: who it is from, the lane that owns it, the paths it touches,
what to do, its state (`open`, `taken`, `applied`, or `refused` with why) and when each change
happened. The owner is a lane; `list` and `stale` resolve it to its live tmux window in session
`blaze-v2` (`<lane>`, or its latest successor `<lane>-next<N>`; the coordinator is `coord`).

Input: one JSON object (inline, a path, or `-`), its `command` choosing the rest:

    {"command": "add", "from_lane": "cargo", "owner": "link", "what": "...", "paths": ["crates/blaze-link/src/model.rs"]}
    {"command": "take", "id": "42", "by": "link-next1"}
    {"command": "done", "id": "42", "by": "link-next1", "note": "applied as written, model.rs:120"}
    {"command": "refuse", "id": "42", "by": "link-next1", "why": "model.rs is codegen's"}
    {"command": "reassign", "id": "42", "by": "coordinator", "owner": "codegen", "why": "codegen owns model.rs"}
    {"command": "list", "owner": "link"}
    {"command": "stale", "minutes": 60}
    {"command": "import"}

`list` shows open and taken requests by owner, oldest first. `stale` shows requests open longer
than `minutes` (since added or last reassigned) with the owner's window or "no live window".
`import` files every open item of `v2/requests/*.md` once (an item is the section under a
heading; it is open when its last `Status:` line or `> applied|answered|done ...` answer says
nothing closed it), and skips items already imported. Every write takes `v2/queue/.lock` and
replaces the file whole, so two agents never interleave.
"""

import dataclasses
import fcntl
import json
import os
import re
import subprocess
import sys
from collections.abc import Callable, Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Literal

from typed_input import InputError, fail, parse, read

SCRIPT = "queue.py"
SESSION = "blaze-v2"
COORDINATOR = "coordinator"
COORDINATOR_WINDOW = "coord"
TREE = str(Path(__file__).resolve().parents[2])

type State = Literal["open", "taken", "applied", "refused"]
type Action = Literal["add", "import", "take", "done", "refuse", "reassign"]
type Format = Literal["text", "json"]


@dataclass(frozen=True)
class Event:
    at: str
    by: str
    action: Action
    note: str = ""


@dataclass(frozen=True)
class Request:
    id: str
    from_lane: str
    owner: str
    paths: tuple[str, ...]
    what: str
    state: State
    created: str
    updated: str
    taken_by: str | None = None
    why: str | None = None
    source: str | None = None
    history: tuple[Event, ...] = ()


@dataclass(frozen=True)
class Add:
    command: Literal["add"]
    from_lane: str
    owner: str
    what: str
    paths: tuple[str, ...] = ()
    root: str = TREE


@dataclass(frozen=True)
class Take:
    command: Literal["take"]
    id: str
    by: str
    root: str = TREE


@dataclass(frozen=True)
class Done:
    command: Literal["done"]
    id: str
    by: str
    note: str
    root: str = TREE


@dataclass(frozen=True)
class Refuse:
    command: Literal["refuse"]
    id: str
    by: str
    why: str
    root: str = TREE


@dataclass(frozen=True)
class Reassign:
    command: Literal["reassign"]
    id: str
    by: str
    owner: str
    why: str
    root: str = TREE


@dataclass(frozen=True)
class List:
    command: Literal["list"]
    owner: str | None = None
    from_lane: str | None = None
    states: tuple[State, ...] = ("open", "taken")
    format: Format = "text"
    root: str = TREE


@dataclass(frozen=True)
class Stale:
    command: Literal["stale"]
    minutes: int
    states: tuple[State, ...] = ("open",)
    format: Format = "text"
    root: str = TREE


@dataclass(frozen=True)
class Import:
    command: Literal["import"]
    requests: str = "v2/requests"
    lanes_from: tuple[str, ...] = (
        str(Path.home() / "restart-2026-10-28/briefs/v2"),
        "/tmp/blaze-lanes",
    )
    dry_run: bool = False
    root: str = TREE


type Command = Add | Take | Done | Refuse | Reassign | List | Stale | Import

COMMANDS: dict[str, type[Command]] = {
    "add": Add,
    "take": Take,
    "done": Done,
    "refuse": Refuse,
    "reassign": Reassign,
    "list": List,
    "stale": Stale,
    "import": Import,
}


class QueueError(Exception):
    """What a command can't do, naming the request."""


def now() -> datetime:
    return datetime.now(UTC).replace(microsecond=0)


def stamp(at: datetime) -> str:
    return at.astimezone(UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


def lane_of(name: str) -> str:
    """The lane a window or agent name belongs to: `link-next3` is lane `link`."""
    name = name.strip().strip("`")
    if name == COORDINATOR_WINDOW:
        return COORDINATOR
    return re.sub(r"-next\d+$", "", name)


def queue_dir(root: str) -> Path:
    return Path(root) / "v2" / "queue"


def normal_id(id: str) -> str:
    if not id.isdigit():
        raise QueueError(f"request id {id!r} is not a number")
    return id.zfill(5)


def load(path: Path) -> Request:
    return parse(Request, str(path), f"{SCRIPT}: {path}")


def load_all(root: str) -> list[Request]:
    folder = queue_dir(root)
    if not folder.is_dir():
        return []
    return [load(p) for p in sorted(folder.glob("*.json"))]


def save(root: str, request: Request) -> None:
    folder = queue_dir(root)
    path = folder / f"{request.id}.json"
    scratch = folder / f".{request.id}.json.tmp"
    scratch.write_text(json.dumps(dataclasses.asdict(request), indent=2, ensure_ascii=False) + "\n")
    os.replace(scratch, path)


@contextmanager
def locked(root: str) -> Iterator[None]:
    folder = queue_dir(root)
    folder.mkdir(parents=True, exist_ok=True)
    with open(folder / ".lock", "w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(lock, fcntl.LOCK_UN)


def next_id(root: str) -> str:
    ids = [int(p.stem) for p in queue_dir(root).glob("*.json") if p.stem.isdigit()]
    return str(max(ids, default=0) + 1).zfill(5)


def get(root: str, id: str) -> Request:
    path = queue_dir(root) / f"{normal_id(id)}.json"
    if not path.is_file():
        raise QueueError(f"no request {normal_id(id)} (no {path})")
    return load(path)


def windows() -> list[tuple[int, str]]:
    """Every window of session `blaze-v2`: (index, name)."""
    out = subprocess.run(
        ["tmux", "list-windows", "-t", SESSION, "-F", "#{window_index}\t#{window_name}"],
        capture_output=True,
        text=True,
    )
    if out.returncode != 0:
        raise QueueError(f"tmux can't list session {SESSION}'s windows: {out.stderr.strip()}")
    rows = [line.split("\t", 1) for line in out.stdout.splitlines() if "\t" in line]
    return [(int(index), name) for index, name in rows]


def window_of(owner: str, live: list[tuple[int, str]]) -> str | None:
    """The owner's live window, its latest successor first: `blaze-v2:57 link-next2`."""
    name = COORDINATOR_WINDOW if owner == COORDINATOR else owner
    successor = re.compile(re.escape(name) + r"-next(\d+)")
    best: tuple[int, int, str] | None = None
    for index, window in live:
        if window == name:
            generation = 0
        elif m := successor.fullmatch(window):
            generation = int(m[1])
        else:
            continue
        if best is None or generation > best[0]:
            best = (generation, index, window)
    return None if best is None else f"{SESSION}:{best[1]} {best[2]}"


def add(c: Add) -> Request:
    if not c.what.strip():
        raise QueueError("`what` is empty: say what to change, where and why")
    if not c.owner.strip() or not c.from_lane.strip():
        raise QueueError("`owner` and `from_lane` name lanes; one of them is empty")
    at = stamp(now())
    with locked(c.root):
        request = Request(
            id=next_id(c.root),
            from_lane=lane_of(c.from_lane),
            owner=lane_of(c.owner),
            paths=c.paths,
            what=c.what,
            state="open",
            created=at,
            updated=at,
            history=(Event(at=at, by=c.from_lane, action="add"),),
        )
        save(c.root, request)
    return request


def change(root: str, id: str, edit: Callable[[Request], Request]) -> Request:
    with locked(root):
        request = edit(get(root, id))
        save(root, request)
    return request



def closed(request: Request) -> QueueError | None:
    if request.state == "applied":
        return QueueError(f"request {request.id} is already applied ({request.updated})")
    if request.state == "refused":
        return QueueError(f"request {request.id} was refused ({request.updated}): {request.why}")
    return None


def take(c: Take) -> Request:
    def edit(r: Request) -> Request:
        if r.state == "taken" and r.taken_by == c.by:
            return r
        if r.state == "taken":
            raise QueueError(f"request {r.id} is taken by {r.taken_by} ({r.updated}); the coordinator reassigns it")
        if err := closed(r):
            raise err
        at = stamp(now())
        return dataclasses.replace(
            r, state="taken", taken_by=c.by, updated=at, history=(*r.history, Event(at=at, by=c.by, action="take"))
        )

    return change(c.root, c.id, edit)


def done(c: Done) -> Request:
    def edit(r: Request) -> Request:
        if err := closed(r):
            raise err
        at = stamp(now())
        return dataclasses.replace(
            r,
            state="applied",
            taken_by=r.taken_by or c.by,
            updated=at,
            history=(*r.history, Event(at=at, by=c.by, action="done", note=c.note)),
        )

    return change(c.root, c.id, edit)


def refuse(c: Refuse) -> Request:
    if not c.why.strip():
        raise QueueError("a refusal says why")

    def edit(r: Request) -> Request:
        if err := closed(r):
            raise err
        at = stamp(now())
        return dataclasses.replace(
            r, state="refused", why=c.why, updated=at, history=(*r.history, Event(at=at, by=c.by, action="refuse", note=c.why))
        )

    return change(c.root, c.id, edit)


def reassign(c: Reassign) -> Request:
    def edit(r: Request) -> Request:
        if r.state == "applied":
            raise QueueError(f"request {r.id} is already applied ({r.updated})")
        at = stamp(now())
        note = f"{r.owner} -> {lane_of(c.owner)}: {c.why}"
        return dataclasses.replace(
            r,
            owner=lane_of(c.owner),
            state="open",
            taken_by=None,
            why=None,
            updated=at,
            history=(*r.history, Event(at=at, by=c.by, action="reassign", note=note)),
        )

    return change(c.root, c.id, edit)


def summary(what: str) -> str:
    line = next((it.strip() for it in what.splitlines() if it.strip()), "")
    line = line.lstrip("#").strip()
    return line if len(line) <= 100 else line[:99] + "…"


def minutes_since(at: str, until: datetime) -> int:
    return int((until - datetime.fromisoformat(at)) / timedelta(minutes=1))


def by_owner(requests: list[Request]) -> list[tuple[str, list[Request]]]:
    """Requests grouped by owner, each group and the groups oldest first."""
    groups: dict[str, list[Request]] = {}
    for r in sorted(requests, key=lambda r: (r.created, r.id)):
        groups.setdefault(r.owner, []).append(r)
    return list(groups.items())


def listing(c: List) -> str:
    chosen = [
        r
        for r in load_all(c.root)
        if r.state in c.states
        and (c.owner is None or r.owner == lane_of(c.owner))
        and (c.from_lane is None or r.from_lane == lane_of(c.from_lane))
    ]
    groups = by_owner(chosen)
    live = windows()
    if c.format == "json":
        return json.dumps(
            [
                {"owner": owner, "window": window_of(owner, live), "requests": [dataclasses.asdict(r) for r in rs]}
                for owner, rs in groups
            ],
            indent=2,
            ensure_ascii=False,
        )
    lines: list[str] = []
    for owner, rs in groups:
        lines.append(f"{owner} ({window_of(owner, live) or 'no live window'}): {len(rs)}")
        for r in rs:
            taker = f" by {r.taken_by}" if r.state == "taken" else ""
            lines.append(f"  {r.id} {r.state}{taker} {r.created} from {r.from_lane}: {summary(r.what)}")
    return "\n".join(lines)


def stale(c: Stale) -> str:
    until = now()
    old = sorted(
        (r for r in load_all(c.root) if r.state in c.states and minutes_since(r.updated, until) > c.minutes),
        key=lambda r: (r.updated, r.id),
    )
    live = windows()
    rows = [(r, minutes_since(r.updated, until), window_of(r.owner, live)) for r in old]
    if c.format == "json":
        return json.dumps(
            [{"id": r.id, "owner": r.owner, "window": w, "minutes": m, "state": r.state, "from_lane": r.from_lane,
              "summary": summary(r.what)} for r, m, w in rows],
            indent=2,
            ensure_ascii=False,
        )
    return "\n".join(
        f"{r.id} {r.state} {m}m owner {r.owner} ({w or 'no live window'}) from {r.from_lane}: {summary(r.what)}"
        for r, m, w in rows
    )


# Importing v2/requests/*.md.

HEADING = re.compile(r"^(#{1,6})\s+(.*?)\s*#*\s*$")
FENCE = re.compile(r"^\s*(?:>\s*)?(```|~~~)")
STATUS = re.compile(r"(?<![`\w])status:\s*\**\s*([a-z]+)", re.IGNORECASE)
ANSWER = re.compile(
    r"^\s*>\s*\**\s*(?:[a-z0-9-]+(?:\s*\([^)]*\))?\s*[:,]\s*)?"
    r"(applied|answered|done|closed|refused|declined|withdrawn|superseded|forwarded|held|taken)\b",
    re.IGNORECASE,
)
OPEN_WORDS = frozenset({"open", "waiting", "partly", "forwarded", "handed", "held", "taken", "pending", "blocked"})
CLOSED_WORDS = frozenset(
    {"done", "applied", "answered", "closed", "withdrawn", "superseded", "read", "fyi", "note", "current",
     "refused", "declined", "wrong", "resolved", "obsolete"}
)
PATH = re.compile(
    r"(?<![\w/.-])((?:crates|tools|docs|v2|harness|python|vendor|fuzz|bench|\.github)/[A-Za-z0-9_./-]*[A-Za-z0-9_]"
    r"|Cargo\.toml|Cargo\.lock)"
)
DATE = re.compile(r"(?<![\w-])(20\d\d-\d\d-\d\d)(?:[ T](\d\d:\d\d))?")
NAME = r"`?([a-z0-9][a-z0-9-]*)`?"
ROUTES = (
    re.compile(r"^\s*>\s*(?:forwarded to|taken by) (?:the )?(?:lane |lead )?" + NAME, re.IGNORECASE | re.MULTILINE),
    re.compile(r"(?<![`\w])status:\s*\**\s*open\b[^\n]*?\((?:to |for )?(?:lane |lead )?" + NAME, re.IGNORECASE),
    re.compile(r"^\s*>?\s*(?:owner|for|to):\s*(?:lane |lead )?" + NAME, re.IGNORECASE | re.MULTILINE),
)
FOR_LEAD = re.compile(r"\bfor (?:lead|lane) " + NAME, re.IGNORECASE)


@dataclass(frozen=True)
class Item:
    file: str
    heading: str
    text: str
    first_line: int


def items(rel: str, text: str) -> list[Item]:
    """The sections of a request file that are requests: those under its shallowest heading below
    the title, or the whole body when it has no such heading."""
    lines = text.splitlines()
    headings: list[tuple[int, int, str]] = []
    fenced = False
    for n, line in enumerate(lines):
        if FENCE.match(line):
            fenced = not fenced
        elif not fenced and (m := HEADING.match(line)):
            headings.append((n, len(m[1]), m[2]))
    if headings and headings[0][1] == 1:
        title_line = headings[0][0]
        headings = headings[1:]
    else:
        title_line = -1
    if not headings:
        body = "\n".join(lines[title_line + 1 :]).strip()
        return [Item(rel, "", body, title_line + 2)] if body else []
    level = min(h[1] for h in headings)
    out: list[Item] = []
    starts = [h for h in headings if h[1] <= level]
    for i, (n, depth, title) in enumerate(starts):
        if depth < level:
            continue
        end = starts[i + 1][0] if i + 1 < len(starts) else len(lines)
        out.append(Item(rel, title, "\n".join(lines[n:end]).rstrip(), n + 1))
    return out


def is_open(text: str) -> bool:
    """Open unless the item's last state marker closed it; an unanswered item is open."""
    state = "open"
    fenced = False
    for line in text.splitlines():
        if FENCE.match(line):
            fenced = not fenced
            continue
        if fenced:
            continue
        for word in [m[1].lower() for m in STATUS.finditer(line)] + [
            m[1].lower() for m in [ANSWER.match(line)] if m
        ]:
            if word in OPEN_WORDS:
                state = "open"
            elif word in CLOSED_WORDS:
                state = "closed"
    return state == "open"


def known_lanes(c: Import) -> set[str]:
    names = {p.stem for p in (Path(c.root) / c.requests).glob("*.md")}
    for folder in c.lanes_from:
        names |= {p.stem.removeprefix("v2-") for p in Path(folder).glob("*.md")}
    names |= {name for _, name in windows()}
    return {lane_of(n) for n in names} | {COORDINATOR}


def from_and_owner(item: Item, stem: str, lanes: set[str]) -> tuple[str, str]:
    prefix = re.match(r"(?:\d+[.)]\s*)?from\s+" + NAME + r"[\s:,(]", item.heading, re.IGNORECASE) or re.match(
        NAME + r"\s*[:,]", item.heading
    )
    author = lane_of(prefix[1]) if prefix and lane_of(prefix[1]) in lanes else stem
    routed = sorted(((m.start(), lane_of(m[1])) for p in ROUTES for m in p.finditer(item.text)), reverse=True)
    hints = [lane for _, lane in routed]
    hints.extend(lane_of(m[1]) for m in FOR_LEAD.finditer(item.text))
    hints.extend(lane_of(m[1]) for m in re.finditer(r"\bto " + NAME, item.heading))
    for hint in hints:
        if hint in lanes and hint != author:
            return author, hint
    return author, stem if author != stem else COORDINATOR


def created_of(item: Item, born: datetime) -> datetime:
    """The item's first date, never before its file was made (an older date is one it cites)."""
    if m := DATE.search(item.text):
        local = datetime.fromisoformat(f"{m[1]}T{m[2] or '00:00'}").astimezone()
        return max(local, born)
    return born


def paths_of(text: str) -> tuple[str, ...]:
    return tuple(dict.fromkeys(m[1].rstrip(".") for m in PATH.finditer(text)))


def import_requests(c: Import) -> list[Request]:
    folder = Path(c.root) / c.requests
    if not folder.is_dir():
        raise QueueError(f"no request folder {folder}")
    lanes = known_lanes(c)
    found: list[tuple[datetime, Item, str, str, str]] = []
    for path in sorted(folder.glob("*.md")):
        rel = str(path.relative_to(c.root))
        info = path.stat()
        born = datetime.fromtimestamp(getattr(info, "st_birthtime", info.st_mtime), UTC)
        seen: dict[str, int] = {}
        for item in items(rel, path.read_text()):
            k = seen[item.heading] = seen.get(item.heading, 0) + 1
            if not is_open(item.text):
                continue
            source = f"{rel}#{item.heading}" + (f"#{k}" if k > 1 else "")
            author, owner = from_and_owner(item, lane_of(path.stem), lanes)
            found.append((created_of(item, born), item, source, author, owner))
    found.sort(key=lambda f: (f[0], f[1].file, f[1].first_line))
    at = stamp(now())
    added: list[Request] = []
    with locked(c.root):
        have = {r.source for r in load_all(c.root) if r.source}
        number = int(next_id(c.root))
        for created, item, source, author, owner in found:
            if source in have:
                continue
            request = Request(
                id=str(number).zfill(5),
                from_lane=author,
                owner=owner,
                paths=paths_of(item.text),
                what=item.text,
                state="open",
                created=stamp(created),
                updated=stamp(created),
                source=source,
                history=(Event(at=at, by="queue-import", action="import", note=f"{item.file}:{item.first_line}"),),
            )
            number += 1
            added.append(request)
            if not c.dry_run:
                save(c.root, request)
    return added


def run(c: Command) -> str:
    match c:
        case Add():
            r = add(c)
            window = window_of(r.owner, windows())
            return f"added {r.id} for {r.owner} ({window or 'no live window; the coordinator will reassign it'})"
        case Take():
            r = take(c)
            return f"{r.id} taken by {r.taken_by}"
        case Done():
            r = done(c)
            return f"{r.id} applied"
        case Refuse():
            r = refuse(c)
            return f"{r.id} refused: {r.why}"
        case Reassign():
            r = reassign(c)
            return f"{r.id} reassigned to {r.owner} ({window_of(r.owner, windows()) or 'no live window'})"
        case List():
            return listing(c)
        case Stale():
            return stale(c)
        case Import():
            added = import_requests(c)
            owners: dict[str, int] = {}
            for r in added:
                owners[r.owner] = owners.get(r.owner, 0) + 1
            verb = "would import" if c.dry_run else "imported"
            per = ", ".join(f"{o} {n}" for o, n in sorted(owners.items(), key=lambda it: -it[1]))
            return f"{verb} {len(added)} open requests" + (f": {per}" if per else "")


def command_of(text: str) -> Command:
    try:
        value = read(text)
    except (InputError, OSError) as e:
        fail(SCRIPT, str(e))
    name = value.get("command") if isinstance(value, dict) else None
    if not isinstance(name, str) or name not in COMMANDS:
        fail(SCRIPT, f"`command` is one of {', '.join(COMMANDS)}, not {json.dumps(name)}")
    return parse(COMMANDS[name], json.dumps(value), SCRIPT)


def main() -> None:
    match sys.argv[1:]:
        case [text]:
            command = command_of(text)
        case _:
            print(f"usage: {SCRIPT} '{{\"command\": \"list\"}}' (see the module doc for each command)", file=sys.stderr)
            sys.exit(2)
    try:
        out = run(command)
    except QueueError as e:
        fail(SCRIPT, str(e))
    if out:
        print(out)


if __name__ == "__main__":
    main()
