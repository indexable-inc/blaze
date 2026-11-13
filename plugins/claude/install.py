#!/usr/bin/env python3
"""Install blaze's after-edit check into an agent's own configuration, without a plugin.

    python3 -I plugins/claude/install.py '{"agent": "claude", "scope": "project"}'
    python3 -I plugins/claude/install.py '{"agent": "claude", "scope": "user", "write": true}'
    python3 -I plugins/claude/install.py --selftest

One JSON value (inline, `-` for stdin, or a path to a file), parsed into `Request`, a frozen
dataclass that refuses fields it does not know (rule scripts-take-typed-input):

    agent    "claude" (default) | "codex" | "generic"
    scope    "project" | "user"
    write    false (default): print the change as a diff and write nothing; true: write it
    home     the home directory (default: the user's); for tests
    project  the repository (default: the checkout this script is in)

What each writes:

    claude  project  <project>/.claude/settings.json   PostToolUse "Write|Edit" -> python3 -I
                     ${CLAUDE_PROJECT_DIR}/plugins/claude/hooks/check.py (the committed path, so
                     every collaborator's checkout runs its own copy)
            user     ~/.claude/settings.json            the same, by this checkout's absolute path
    codex   project  <project>/.codex/hooks.json        PostToolUse "apply_patch" -> python3 -I
                     "$(git rev-parse --show-toplevel)/plugins/codex/check.py" (the form Codex's
                     docs give for a repo-local hook)
            user     ~/.codex/hooks.json                the same, by this checkout's absolute path
    generic user     ~/.local/bin/blaze-new-diagnostics, a link to plugins/agent/blaze-new-diagnostics,
                     and prints the line for the agent's instructions file (plugins/agent/README.md)

A settings file is read, merged and written atomically (a temporary file in the same directory,
fsync, rename): the hook goes into the event's group with the same matcher, or a new group; a
handler of this hook already there is replaced, any other handler and every other key is kept as
it was. A file that is not a JSON object, or whose `hooks` is not shaped as the docs say, is
refused naming the file and the key, never overwritten.
"""

from __future__ import annotations

import dataclasses
import difflib
import json
import os
import shlex
import sys
import tempfile
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
CHECKOUT = HERE.parents[1]
TIMEOUT_S = 30  # over the adapters' 20 s deadline: plugins/claude/hooks/hooks.json says why
AGENTS = ("claude", "codex", "generic")
SCOPES = ("project", "user")
OURS = ("plugins/claude/hooks/check.py", "plugins/codex/check.py")
INSTRUCTION = (
    "After you edit a Rust file or a Cargo.toml, run `blaze-new-diagnostics <file>` and fix the "
    "errors it prints before going on; it prints only what is new since its last run."
)


class Refused(Exception):
    pass


@dataclasses.dataclass(frozen=True)
class Request:
    scope: str
    agent: str = "claude"
    write: bool = False
    home: str | None = None
    project: str | None = None

    @staticmethod
    def parse(value: Any) -> "Request":
        if not isinstance(value, dict):
            raise Refused(f"the input is one JSON object, not {type(value).__name__}")
        known = {f.name for f in dataclasses.fields(Request)}
        unknown = sorted(set(value) - known)
        if unknown:
            raise Refused(f"unknown field(s) {', '.join(unknown)}: the fields are {', '.join(sorted(known))}")
        if value.get("scope") not in SCOPES:
            raise Refused(f"`scope` is one of {', '.join(SCOPES)}, not {value.get('scope')!r}")
        if value.get("agent", "claude") not in AGENTS:
            raise Refused(f"`agent` is one of {', '.join(AGENTS)}, not {value.get('agent')!r}")
        if not isinstance(value.get("write", False), bool):
            raise Refused("`write` is true or false")
        for name in ("home", "project"):
            if value.get(name) is not None and not isinstance(value[name], str):
                raise Refused(f"`{name}` is a path or null")
        if value.get("agent") == "generic" and value["scope"] == "project":
            raise Refused(
                "a generic agent has no project hook to install: give `\"scope\": \"user\"` to link "
                "blaze-new-diagnostics into ~/.local/bin, and add the line plugins/agent/README.md gives "
                "to the project's instructions file yourself"
            )
        return Request(value["scope"], value.get("agent", "claude"), value.get("write", False), value.get("home"), value.get("project"))


@dataclasses.dataclass(frozen=True)
class Plan:
    target: Path
    event: str
    matcher: str
    handler: dict[str, Any]


def plan(req: Request) -> Plan:
    home = Path(req.home) if req.home else Path.home()
    project = Path(req.project) if req.project else CHECKOUT
    if req.agent == "claude":
        if req.scope == "project":
            script, target = "${CLAUDE_PROJECT_DIR}/plugins/claude/hooks/check.py", project / ".claude" / "settings.json"
        else:
            script, target = str(CHECKOUT / "plugins/claude/hooks/check.py"), home / ".claude" / "settings.json"
        handler = {"type": "command", "command": "python3", "args": ["-I", script], "timeout": TIMEOUT_S}
        return Plan(target, "PostToolUse", "Write|Edit", handler)
    if req.scope == "project":
        command, target = 'python3 -I "$(git rev-parse --show-toplevel)/plugins/codex/check.py"', project / ".codex" / "hooks.json"
    else:
        command, target = "python3 -I " + shlex.quote(str(CHECKOUT / "plugins/codex/check.py")), home / ".codex" / "hooks.json"
    handler = {"type": "command", "command": command, "timeout": TIMEOUT_S, "statusMessage": "blaze check"}
    return Plan(target, "PostToolUse", "apply_patch", handler)


def is_ours(handler: Any) -> bool:
    if not isinstance(handler, dict):
        return False
    words = [handler.get("command", "")] + [a for a in handler.get("args", []) if isinstance(a, str)]
    return any(isinstance(w, str) and any(o in w for o in OURS) for w in words)


def merged(doc: dict[str, Any], p: Plan) -> dict[str, Any]:
    out = json.loads(json.dumps(doc))  # a deep copy; the input is left as read
    hooks = out.setdefault("hooks", {})
    if not isinstance(hooks, dict):
        raise Refused(f"{p.target}: `hooks` is {type(hooks).__name__}, not an object of events")
    groups = hooks.setdefault(p.event, [])
    if not isinstance(groups, list):
        raise Refused(f"{p.target}: `hooks.{p.event}` is {type(groups).__name__}, not a list of matcher groups")
    for group in groups:
        if isinstance(group, dict) and group.get("matcher") == p.matcher:
            handlers = group.setdefault("hooks", [])
            if not isinstance(handlers, list):
                raise Refused(f"{p.target}: `hooks.{p.event}[matcher={p.matcher}].hooks` is not a list")
            group["hooks"] = [h for h in handlers if not is_ours(h)]
            # ours replaces ours where it stood, else goes last
            at = next((i for i, h in enumerate(handlers) if is_ours(h)), len(group["hooks"]))
            group["hooks"].insert(at, p.handler)
            return out
    groups.append({"matcher": p.matcher, "hooks": [p.handler]})
    return out


def read_doc(path: Path) -> tuple[dict[str, Any], str]:
    try:
        text = path.read_text(encoding="utf-8")
    except FileNotFoundError:
        return {}, ""
    try:
        doc = json.loads(text) if text.strip() else {}
    except json.JSONDecodeError as error:
        raise Refused(f"{path} is not JSON ({error}): fix it or move it aside; it was not changed")
    if not isinstance(doc, dict):
        raise Refused(f"{path} is a JSON {type(doc).__name__}, not an object; it was not changed")
    return doc, text


def write_atomic(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=path.name + ".", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(text)
            f.flush()
            os.fsync(f.fileno())
        if path.exists():
            os.chmod(tmp, path.stat().st_mode & 0o777)
        os.replace(tmp, path)
    except BaseException:
        if os.path.exists(tmp):
            os.unlink(tmp)
        raise


def install_hook(req: Request) -> str:
    p = plan(req)
    doc, old_text = read_doc(p.target)
    new_doc = merged(doc, p)
    new_text = json.dumps(new_doc, indent=2) + "\n"
    if new_doc == doc:
        return f"{p.target}: the blaze hook is already there; nothing to change."
    before = json.dumps(doc, indent=2) + "\n" if doc else ""
    diff = "".join(difflib.unified_diff(before.splitlines(True), new_text.splitlines(True), str(p.target), str(p.target) + " (new)"))
    lines = [diff.rstrip("\n")]
    if old_text and before != old_text:
        lines.append(f"(the file is rewritten as 2-space JSON; its keys and values are kept)")
    if req.write:
        write_atomic(p.target, new_text)
        lines.append(f"wrote {p.target}.")
    else:
        lines.append('dry run: nothing written; add "write": true to apply.')
    if req.agent == "codex":
        lines.append("Codex runs a new hook only once you trust it: open /hooks in Codex and trust it.")
    return "\n".join(lines)


def install_generic(req: Request) -> str:
    home = Path(req.home) if req.home else Path.home()
    link = home / ".local" / "bin" / "blaze-new-diagnostics"
    target = CHECKOUT / "plugins" / "agent" / "blaze-new-diagnostics"
    lines = []
    if link.is_symlink() and os.readlink(link) == str(target):
        lines.append(f"{link} already links {target}.")
    elif link.exists() and not link.is_symlink():
        raise Refused(f"{link} is a file, not this launcher's link; it was not changed")
    else:
        lines.append(f"+ {link} -> {target}")
        if req.write:
            link.parent.mkdir(parents=True, exist_ok=True)
            tmp = link.with_name(link.name + f".{os.getpid()}.tmp")
            os.symlink(target, tmp)
            os.replace(tmp, link)
            lines.append(f"linked {link}.")
        else:
            lines.append('dry run: nothing written; add "write": true to apply.')
    on_path = str(link.parent) in os.environ.get("PATH", "").split(os.pathsep)
    if not on_path:
        lines.append(f"{link.parent} is not on PATH: add it, or call {target} by its path.")
    lines.append("Add this line to the agent's instructions file (AGENTS.md, or what it reads):")
    lines.append(f"  {INSTRUCTION}")
    return "\n".join(lines)


def run(req: Request) -> str:
    return install_generic(req) if req.agent == "generic" else install_hook(req)


def read_value(arg: str) -> Any:
    if arg == "-":
        return json.load(sys.stdin)
    if arg.lstrip().startswith("{"):
        return json.loads(arg)
    return json.loads(Path(arg).read_text(encoding="utf-8"))


def main(argv: list[str]) -> int:
    if argv == ["--selftest"]:
        return selftest()
    if len(argv) != 1:
        print("usage: install.py '<json>' | - | FILE | --selftest (fields: agent, scope, write, home, project)", file=sys.stderr)
        return 2
    try:
        print(run(Request.parse(read_value(argv[0]))))
    except (Refused, json.JSONDecodeError, OSError) as error:
        print(f"install.py: {error}", file=sys.stderr)
        return 1
    return 0


def selftest() -> int:
    failures: list[str] = []

    def expect(name: str, got: Any, want: Any) -> None:
        if got != want:
            failures.append(f"{name}: got {got!r}, want {want!r}")

    with tempfile.TemporaryDirectory(prefix="blaze-install-selftest-") as tmp:
        home, project = Path(tmp) / "home", Path(tmp) / "project"
        settings = project / ".claude" / "settings.json"
        settings.parent.mkdir(parents=True)
        existing = {
            "permissions": {"allow": ["Bash(ls:*)"]},
            "hooks": {
                "PostToolUse": [
                    {"matcher": "Bash", "hooks": [{"type": "command", "command": "log.sh"}]},
                    {"matcher": "Write|Edit", "hooks": [{"type": "command", "command": "prettier.sh"}]},
                ],
                "Stop": [{"hooks": [{"type": "command", "command": "stop.sh"}]}],
            },
        }
        settings.write_text(json.dumps(existing))
        base = {"home": str(home), "project": str(project)}

        out = run(Request.parse({"agent": "claude", "scope": "project", **base}))
        expect("dry.unchanged", json.loads(settings.read_text()), existing)
        expect("dry.says", "dry run" in out and any(l.startswith("+") and '"args": [' in l for l in out.splitlines()), True)
        run(Request.parse({"agent": "claude", "scope": "project", "write": True, **base}))
        doc = json.loads(settings.read_text())
        expect("kept.permissions", doc["permissions"], existing["permissions"])
        expect("kept.stop", doc["hooks"]["Stop"], existing["hooks"]["Stop"])
        expect("kept.bash", doc["hooks"]["PostToolUse"][0], existing["hooks"]["PostToolUse"][0])
        group = doc["hooks"]["PostToolUse"][1]["hooks"]
        expect("merged.same-matcher", [h.get("command") for h in group], ["prettier.sh", "python3"])
        expect("merged.args", group[1]["args"], ["-I", "${CLAUDE_PROJECT_DIR}/plugins/claude/hooks/check.py"])
        out = run(Request.parse({"agent": "claude", "scope": "project", "write": True, **base}))
        expect("idempotent", "already there" in out, True)
        expect("idempotent.count", len(json.loads(settings.read_text())["hooks"]["PostToolUse"][1]["hooks"]), 2)

        run(Request.parse({"agent": "claude", "scope": "user", "write": True, **base}))
        user = json.loads((home / ".claude" / "settings.json").read_text())
        expect("user.path", user["hooks"]["PostToolUse"][0]["hooks"][0]["args"][1], str(CHECKOUT / "plugins/claude/hooks/check.py"))

        run(Request.parse({"agent": "codex", "scope": "user", "write": True, **base}))
        codex = json.loads((home / ".codex" / "hooks.json").read_text())
        h = codex["hooks"]["PostToolUse"][0]
        expect("codex.matcher", h["matcher"], "apply_patch")
        expect("codex.command", h["hooks"][0]["command"].endswith("plugins/codex/check.py") or h["hooks"][0]["command"].endswith("plugins/codex/check.py'"), True)
        out = run(Request.parse({"agent": "codex", "scope": "project", **base}))
        expect("codex.project", "git rev-parse --show-toplevel" in out and "trust" in out, True)

        out = run(Request.parse({"agent": "generic", "scope": "user", **base}))
        expect("generic.dry", (home / ".local/bin/blaze-new-diagnostics").exists(), False)
        run(Request.parse({"agent": "generic", "scope": "user", "write": True, **base}))
        link = home / ".local/bin/blaze-new-diagnostics"
        expect("generic.link", os.readlink(link), str(CHECKOUT / "plugins/agent/blaze-new-diagnostics"))
        expect("generic.again", "already links" in run(Request.parse({"agent": "generic", "scope": "user", **base})), True)

        bad = project / ".codex" / "hooks.json"
        bad.parent.mkdir(parents=True)
        bad.write_text('{"hooks": []}')
        try:
            run(Request.parse({"agent": "codex", "scope": "project", "write": True, **base}))
            failures.append("refuse: a `hooks` list was overwritten")
        except Refused as error:
            expect("refuse.names", "hooks" in str(error) and str(bad) in str(error), True)
        expect("refuse.untouched", bad.read_text(), '{"hooks": []}')
        for value, word in (({"scope": "user", "colour": 1}, "colour"), ({"scope": "everywhere"}, "scope"), ({"agent": "generic", "scope": "project"}, "generic")):
            try:
                Request.parse(value)
                failures.append(f"request: {value} accepted")
            except Refused as error:
                expect(f"request.{word}", word in str(error), True)

    for f in failures:
        print(f"FAIL {f}")
    print(f"install selftest: {'ok' if not failures else 'FAILED'} ({len(failures)} failure(s))")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
