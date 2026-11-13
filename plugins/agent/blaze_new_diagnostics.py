#!/usr/bin/env python3
"""blaze-new-diagnostics: after an edit, the errors and warnings blaze has that are new.

The agent-neutral core of blaze's coding-agent plugins (plugins/claude, plugins/codex,
plugins/agent/README.md, docs/claude-plugin.md). One edited file in; out, what `blaze check` of
the package that owns it says that the last check of that package in the same session did not.

    python3 -I blaze_new_diagnostics.py '<json>'     one JSON value inline, or `-` for stdin
    python3 -I blaze_new_diagnostics.py --text FILE  the shorthand an agent types at a shell
    python3 -I blaze_new_diagnostics.py --selftest   fixtures against a fake blaze

The input is one JSON value parsed into `Input` (a frozen dataclass that refuses fields it does
not know, rule scripts-take-typed-input):

    {"file": "src/lib.rs",           the file just written, absolute or relative to cwd
     "cwd": "/w",                    the agent's working directory
     "session": "abc123",            the agent's session id: "new" is new since the last check
                                     of the same package under the same session
     "deadline_s": 20,               how long blaze may take before the answer is "still running"
     "state_dir": null,              where the session's state lives (default: the system temp
                                     directory's blaze-new-diagnostics/)
     "blaze": null,                  the blaze to run (default: `blaze` on PATH)
     "format": "json"}               "json" (one JSON value, `Result`) or "text"

`--text FILE` is `{"file": FILE, "cwd": <the shell's>, "session": "shell", "deadline_s": 120,
"format": "text"}` with `blaze` from `BLAZE_BIN` when set: the one shorthand, so the line an
agent's instructions carry (plugins/agent/README.md) stays one short command.

What it does, in order:

1. Acts only for a `.rs` file or a `Cargo.toml` with a `Cargo.toml` at or above it; otherwise
   `skipped`, and the adapters print nothing.
2. No blaze to run: `skipped` too, unless the workspace pins blaze (`blaze.lock`, or
   `[workspace.metadata.blaze]` in its root manifest); then `no_blaze`, once a session (a marker
   in the session's state directory), naming what is missing.
3. Runs `blaze check --manifest-path <the owning package's Cargo.toml> -p <its name>
   --all-targets --message-format json --color never`: the narrowest check blaze has (one
   package; blaze has no check of one file's items). `--all-targets` because an edit to a test,
   an example or a `#[cfg(test)]` module is only checked with it. A virtual manifest checks
   `--workspace`. The run is bounded by `deadline_s`; at the deadline the client is left running
   in its own session (its output goes to a file, so nothing it writes fails) and the answer is
   `timeout`, saying so.
4. Reads the JSON messages: `compiler-message` and the closing `build-finished` (docs/cargo.md,
   "check"). Errors of any package (they fail the check), warnings of the checked package alone
   (a dependency's are counted by blaze, never drawn: docs/terminal.md, "Off a terminal").
   A diagnostic said once per target (the library and its unit tests) is one.
5. Diffs against the session's last check of the same package and keeps this one as the next
   base. A diagnostic's identity is its level, its code, its file (workspace-relative), its
   message with line references normalized (`:12:5`, `line 12`), the text of its primary span's
   first source line and that span's label. Not the line number: one inserted line moves every
   diagnostic below it, and a key with the line would report each of them as fixed and new
   again. The text it points at moves with it. Identities are counted as a multiset, so the
   same warning on two lines is two. A run that timed out or where blaze failed keeps the base.
6. The first check of a package in a session shows the diagnostics of the edited file and counts
   the rest; later ones show what is new anywhere in the package, and count what was fixed and
   what is unchanged.

Never quiet about blaze itself (rule no-fallback): a blaze that cannot start, panics, dies,
refuses its command line, prints no `build-finished`, or says the build failed without naming an
error is `blaze_failed`, with blaze's own last lines, said as blaze failing, not the code.

The output, `Result` (one JSON value), always has every field:

    {"status": "checked" | "skipped" | "no_blaze" | "timeout" | "blaze_failed",
     "file", "package", "manifest", "rerun",       what was checked, and the command to see all
     "first_run": bool,
     "errors": [Diag], "warnings": [Diag],          new (first run: in the edited file)
     "more_errors": N, "more_warnings": N,          new ones past the cap
     "fixed": N, "unchanged": N,
     "elsewhere": {"errors": N, "warnings": N},     first run: the rest of the package
     "blaze_failed": null | "...",                  blaze's own last lines
     "note": null | "...",                          a timeout, no blaze, an unreadable base
     "text": "..."}                                 all of it as the lines an agent reads

    Diag: {"path", "line", "col", "level", "code", "message", "help": [...], "line_text"}

Each diagnostic is drawn `path:line:col: level[code]: message` with blaze's help lines under it;
at most CAP of each level are drawn and the text of each kind stays under TEXT_BUDGET characters
(Claude Code puts more than 10,000 characters of hook context in a file and shows Claude 2,000 of
them; Codex spills past about 2,500 tokens: plugins/claude/FORMAT-NOTES.md,
plugins/codex/FORMAT-NOTES.md), the rest as "and N more: run `blaze check ...`".

Standard library only, Python 3.11 or later (tomllib). The pieces that decide (what owns a file,
what a message means, identity, the diff, the text) are pure functions; `run` is the thin driver
that reads files, the clock and the process.
"""

from __future__ import annotations

import collections
import dataclasses
import errno
import fcntl
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib
from pathlib import Path
from typing import Any, Callable, Iterable

CAP = 10
TEXT_BUDGET = 8000
TAIL_LINES = 15
STATE_FORMAT = 1
FORMATS = ("json", "text")


class InputError(Exception):
    """The input is not one this script takes: named, with the field."""


@dataclasses.dataclass(frozen=True)
class Input:
    file: str
    cwd: str
    session: str
    deadline_s: float
    state_dir: str | None = None
    blaze: str | None = None
    format: str = "json"

    @staticmethod
    def parse(value: Any) -> "Input":
        if not isinstance(value, dict):
            raise InputError(f"the input is one JSON object, not {type(value).__name__}")
        known = {f.name for f in dataclasses.fields(Input)}
        unknown = sorted(set(value) - known)
        if unknown:
            raise InputError(f"unknown field(s) {', '.join(unknown)}: the fields are {', '.join(sorted(known))}")
        for name in ("file", "cwd", "session", "deadline_s"):
            if name not in value:
                raise InputError(f"missing field `{name}`")
        for name in ("file", "cwd", "session"):
            if not isinstance(value[name], str) or not value[name]:
                raise InputError(f"`{name}` is a non-empty string")
        deadline = value["deadline_s"]
        if isinstance(deadline, bool) or not isinstance(deadline, (int, float)) or deadline <= 0:
            raise InputError("`deadline_s` is a number of seconds above 0")
        for name in ("state_dir", "blaze"):
            if value.get(name) is not None and not isinstance(value[name], str):
                raise InputError(f"`{name}` is a string or null")
        fmt = value.get("format", "json")
        if fmt not in FORMATS:
            raise InputError(f"`format` is one of {', '.join(FORMATS)}, not {fmt!r}")
        return Input(
            file=value["file"],
            cwd=value["cwd"],
            session=value["session"],
            deadline_s=float(deadline),
            state_dir=value.get("state_dir"),
            blaze=value.get("blaze"),
            format=fmt,
        )


# ---------------------------------------------------------------------------------------------
# What owns a file.


@dataclasses.dataclass(frozen=True)
class Owner:
    """The check an edit of `file` asks for."""

    file: Path
    manifest: Path  # the owning package's Cargo.toml, or the virtual workspace's
    package: str | None  # None: a virtual manifest (`--workspace`), or one that does not parse
    workspace_root: Path
    unparsed: str | None  # why the manifest could not be read, when it could not

    def args(self) -> list[str]:
        args = ["check", "--manifest-path", str(self.manifest)]
        if self.package is not None:
            args += ["-p", self.package]
        elif self.unparsed is None:
            args += ["--workspace"]
        return args + ["--all-targets", "--message-format", "json", "--color", "never"]

    def rerun(self) -> str:
        if self.package is not None:
            return f"blaze check -p {self.package} --all-targets"
        if self.unparsed is None:
            return "blaze check --workspace --all-targets"
        return f"blaze check --manifest-path {self.manifest} --all-targets"

    def name(self) -> str:
        if self.package is not None:
            return self.package
        if self.unparsed is None:
            return f"the workspace at {self.workspace_root}"
        return str(self.manifest)


def read_toml(path: Path) -> tuple[dict[str, Any] | None, str | None]:
    try:
        return tomllib.loads(path.read_text(encoding="utf-8")), None
    except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
        return None, f"{path} does not read as TOML: {error}"


def owner_of(file: Path) -> Owner | None:
    """The package that owns `file`, or None when nothing does (no Cargo.toml at or above it)."""
    if not (file.suffix == ".rs" or file.name == "Cargo.toml"):
        return None
    start = file.parent
    manifests = [d / "Cargo.toml" for d in [start, *start.parents] if (d / "Cargo.toml").is_file()]
    if not manifests:
        return None
    nearest = manifests[0]
    owner_manifest, package, unparsed = None, None, None
    for manifest in manifests:
        doc, why = read_toml(manifest)
        if doc is None:
            # The manifest just edited may not parse: blaze is the one to say where, so it is
            # checked by path, and the reason is kept for when blaze cannot say it either.
            owner_manifest, unparsed = manifest, why
            break
        if isinstance(doc.get("package"), dict):
            owner_manifest, package = manifest, doc["package"].get("name")
            if not isinstance(package, str):
                package = None
            break
        if "workspace" in doc:
            owner_manifest = manifest
            break
    if owner_manifest is None:
        owner_manifest = nearest
    root = workspace_root(owner_manifest)
    return Owner(file, owner_manifest, package, root, unparsed)


def workspace_root(manifest: Path) -> Path:
    """The nearest directory at or above `manifest` whose Cargo.toml has `[workspace]`: where
    blaze runs rustc's part from, which relative file names in diagnostics are relative to."""
    for d in [manifest.parent, *manifest.parent.parents]:
        candidate = d / "Cargo.toml"
        if candidate.is_file():
            doc, _ = read_toml(candidate)
            if doc is not None and "workspace" in doc:
                return d
    return manifest.parent


def pins_blaze(root: Path) -> str | None:
    """What says this workspace is built with blaze, or None."""
    if (root / "blaze.lock").is_file():
        return f"{root / 'blaze.lock'}"
    doc, _ = read_toml(root / "Cargo.toml")
    if doc is not None:
        meta = doc.get("workspace", {}).get("metadata", {}) if isinstance(doc.get("workspace"), dict) else {}
        if isinstance(meta, dict) and "blaze" in meta:
            return f"[workspace.metadata.blaze] in {root / 'Cargo.toml'}"
    return None


# ---------------------------------------------------------------------------------------------
# What blaze said.


@dataclasses.dataclass(frozen=True)
class Diag:
    path: str  # absolute
    line: int
    col: int
    level: str
    code: str | None
    message: str
    help: tuple[str, ...]
    line_text: str
    label: str
    own: bool  # of the checked package (or workspace), not a dependency

    def key(self, root: Path) -> tuple[str, ...]:
        return (
            self.level,
            self.code or "",
            relative(self.path, root),
            normalize(self.message),
            self.line_text.strip(),
            normalize(self.label),
        )


LINE_REFS = re.compile(r"(?::\d+:\d+\b)|(?:\blines? \d+(?:\s*(?:-|to)\s*\d+)?\b)")


def normalize(text: str) -> str:
    return LINE_REFS.sub("#", text)


def relative(path: str, root: Path) -> str:
    try:
        return Path(path).relative_to(root).as_posix()
    except ValueError:
        return Path(path).as_posix()


def resolve_span_file(name: str, manifest_dir: Path, root: Path) -> str:
    """A span's `file_name` as blaze writes it: absolute, or relative to the directory rustc ran
    in, an ancestor of the unit's manifest directory no higher than the workspace root: the
    nearest that has it (`Sources::file`, crates/blaze-cargo/src/cli/json.rs)."""
    p = Path(name)
    if p.is_absolute():
        return str(p)
    for d in [manifest_dir, *manifest_dir.parents]:
        if not str(d).startswith(str(root)):
            break
        if (d / p).exists():
            return str(d / p)
    return str(root / p)


@dataclasses.dataclass(frozen=True)
class Answer:
    """One `blaze check`'s stdout, read."""

    diags: tuple[Diag, ...]
    finished: dict[str, Any] | None
    not_json: int


def read_messages(stdout: str, owner: Owner) -> Answer:
    diags: list[Diag] = []
    seen: set[tuple[Any, ...]] = set()
    finished = None
    not_json = 0
    own_manifest = os.path.realpath(owner.manifest)
    for line in stdout.splitlines():
        if not line.strip():
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            not_json += 1
            continue
        if not isinstance(message, dict):
            not_json += 1
            continue
        reason = message.get("reason")
        if reason == "build-finished":
            finished = message
            continue
        if reason != "compiler-message":
            continue
        diagnostic = message.get("message") or {}
        level = diagnostic.get("level")
        if level not in ("error", "warning", "error: internal compiler error"):
            continue
        level = "error" if level.startswith("error") else "warning"
        manifest_path = message.get("manifest_path") or ""
        manifest_real = os.path.realpath(manifest_path) if manifest_path else ""
        if owner.package is None and owner.unparsed is None:
            own = manifest_real.startswith(str(owner.workspace_root) + os.sep)
        else:
            own = manifest_real == own_manifest
        if level == "warning" and not own:
            continue
        spans = diagnostic.get("spans") or []
        primary = next((s for s in spans if s.get("is_primary")), spans[0] if spans else None)
        manifest_dir = Path(manifest_path).parent if manifest_path else owner.manifest.parent
        if primary is not None:
            path = resolve_span_file(primary.get("file_name", ""), manifest_dir, owner.workspace_root)
            line_no, col = int(primary.get("line_start") or 0), int(primary.get("column_start") or 0)
            texts = primary.get("text") or []
            line_text = texts[0].get("text", "") if texts and isinstance(texts[0], dict) else ""
            label = primary.get("label") or ""
        else:
            path, line_no, col, line_text, label = manifest_path or str(owner.manifest), 0, 0, "", ""
        code_obj = diagnostic.get("code")
        code = code_obj.get("code") if isinstance(code_obj, dict) else None
        helps = tuple(
            c.get("message", "")
            for c in diagnostic.get("children") or []
            if isinstance(c, dict) and c.get("level") == "help" and c.get("message")
        )
        d = Diag(path, line_no, col, level, code, diagnostic.get("message", ""), helps, line_text, label, own)
        place = (d.level, d.code, d.path, d.line, d.col, d.message)
        if place in seen:
            continue  # the same diagnostic of another target of the package (lib and lib test)
        seen.add(place)
        diags.append(d)
    return Answer(tuple(diags), finished, not_json)


def blaze_failure(exit_code: int, answer: Answer, stderr: str) -> str | None:
    """Why this answer is blaze failing rather than the code, or None when it is an answer."""
    tail = "\n".join(stderr.rstrip().splitlines()[-TAIL_LINES:])
    errors = sum(1 for d in answer.diags if d.level == "error")

    def said(why: str) -> str:
        return f"{why}; blaze's last lines on stderr:\n{tail}" if tail else f"{why}; blaze wrote nothing on stderr"

    if answer.finished is None:
        return said(f"blaze exited {exit_code} without its closing `build-finished` message")
    if answer.not_json:
        return said(f"blaze wrote {answer.not_json} line(s) on stdout that are not JSON messages")
    success = answer.finished.get("success")
    if success is False and errors == 0:
        return said(f"blaze said the check failed (exit {exit_code}) and named no error")
    if success is True and exit_code != 0:
        return said(f"blaze said the check succeeded but exited {exit_code}")
    if success is True and errors:
        return said(f"blaze said the check succeeded but sent {errors} error(s)")
    return None


# ---------------------------------------------------------------------------------------------
# The diff and the text.


def diff(
    current: list[Diag], base: collections.Counter | None, edited: Path, root: Path
) -> dict[str, Any]:
    """`base` None: the first check of the package in the session."""
    keys = [d.key(root) for d in current]
    if base is None:
        edited_real = os.path.realpath(edited)
        shown = [d for d in current if os.path.realpath(d.path) == edited_real]
        rest = [d for d in current if os.path.realpath(d.path) != edited_real]
        return {
            "first_run": True,
            "new": shown,
            "fixed": 0,
            "unchanged": 0,
            "unchanged_errors": 0,
            "elsewhere": {
                "errors": sum(d.level == "error" for d in rest),
                "warnings": sum(d.level == "warning" for d in rest),
            },
            "keys": collections.Counter(keys),
        }
    now = collections.Counter(keys)
    left = collections.Counter(base)
    new: list[Diag] = []
    for d, k in zip(current, keys):
        if left[k] > 0:
            left[k] -= 1
        else:
            new.append(d)
    return {
        "first_run": False,
        "new": new,
        "fixed": sum((base - now).values()),
        "unchanged": sum((base & now).values()),
        "unchanged_errors": sum(n for k, n in (base & now).items() if k[0] == "error"),
        "elsewhere": {"errors": 0, "warnings": 0},
        "keys": now,
    }


def shown_path(path: str, cwd: Path) -> str:
    try:
        return Path(path).relative_to(cwd).as_posix()
    except ValueError:
        return path


def draw(d: Diag, cwd: Path) -> str:
    code = f"[{d.code}]" if d.code else ""
    lines = [f"{shown_path(d.path, cwd)}:{d.line}:{d.col}: {d.level}{code}: {d.message}"]
    lines += [f"  help: {h}" for h in d.help]
    return "\n".join(lines)


def capped(diags: list[Diag], cwd: Path) -> tuple[list[str], int]:
    """At most CAP drawn, within TEXT_BUDGET characters; how many more."""
    out: list[str] = []
    used = 0
    for d in diags[:CAP]:
        text = draw(d, cwd)
        if used + len(text) + 1 > TEXT_BUDGET:
            break
        out.append(text)
        used += len(text) + 1
    return out, len(diags) - len(out)


def plural(n: int, word: str) -> str:
    return f"{n} {word}" if n == 1 else f"{n} {word}s"


def diag_json(d: Diag) -> dict[str, Any]:
    return {
        "path": d.path,
        "line": d.line,
        "col": d.col,
        "level": d.level,
        "code": d.code,
        "message": d.message,
        "help": list(d.help),
        "line_text": d.line_text,
    }


def empty_result(status: str, file: Path, owner: Owner | None) -> dict[str, Any]:
    return {
        "status": status,
        "file": str(file),
        "package": owner.package if owner else None,
        "manifest": str(owner.manifest) if owner else None,
        "rerun": owner.rerun() if owner else None,
        "first_run": False,
        "errors": [],
        "warnings": [],
        "more_errors": 0,
        "more_warnings": 0,
        "fixed": 0,
        "unchanged": 0,
        "unchanged_errors": 0,
        "elsewhere": {"errors": 0, "warnings": 0},
        "blaze_failed": None,
        "note": None,
        "text": "",
    }


def checked_result(file: Path, owner: Owner, d: dict[str, Any], cwd: Path, note: str | None) -> dict[str, Any]:
    result = empty_result("checked", file, owner)
    errors = [x for x in d["new"] if x.level == "error"]
    warnings = [x for x in d["new"] if x.level == "warning"]
    drawn_errors, more_errors = capped(errors, cwd)
    drawn_warnings, more_warnings = capped(warnings, cwd)
    result.update(
        first_run=d["first_run"],
        errors=[diag_json(x) for x in errors[: len(drawn_errors)]],
        warnings=[diag_json(x) for x in warnings[: len(drawn_warnings)]],
        more_errors=more_errors,
        more_warnings=more_warnings,
        fixed=d["fixed"],
        unchanged=d["unchanged"],
        unchanged_errors=d["unchanged_errors"],
        elsewhere=d["elsewhere"],
        note=note,
    )
    result["text"] = render(result, drawn_errors, drawn_warnings)
    result["error_text"] = section(result, "error", drawn_errors, more_errors)
    result["warning_text"] = section(result, "warning", drawn_warnings, more_warnings)
    result["summary"] = summary(result)
    return result


def section(result: dict[str, Any], level: str, drawn: list[str], more: int) -> str:
    if not drawn and not more:
        return ""
    where = f"in {shown_name(result)}" if not result["first_run"] else f"in {Path(result['file']).name}"
    head = f"blaze check {result_scope(result)}: {plural(len(drawn) + more, 'new ' + level)} {where}:"
    if result["first_run"]:
        head = f"blaze check {result_scope(result)}: {plural(len(drawn) + more, level)} {where}:"
    lines = [head, *drawn]
    if more:
        lines.append(f"and {more} more: run `{result['rerun']}`")
    return "\n".join(lines)


def shown_name(result: dict[str, Any]) -> str:
    return result["package"] or result["manifest"]


def result_scope(result: dict[str, Any]) -> str:
    return f"-p {result['package']}" if result["package"] else f"--manifest-path {result['manifest']}"


def summary(result: dict[str, Any]) -> str:
    scope = result_scope(result)
    if result["first_run"]:
        el = result["elsewhere"]
        if not el["errors"] and not el["warnings"]:
            return f"blaze check {scope}: first check this session; nothing elsewhere in {shown_name(result)}."
        return (
            f"blaze check {scope}: first check this session; elsewhere in {shown_name(result)}: "
            f"{plural(el['errors'], 'error')}, {plural(el['warnings'], 'warning')} (`{result['rerun']}` shows them)."
        )
    new = len(result["errors"]) + result["more_errors"] + len(result["warnings"]) + result["more_warnings"]
    parts = [f"{new} new" if new else "nothing new"]
    parts.append(f"{result['fixed']} fixed")
    parts.append(f"{result['unchanged']} unchanged")
    line = f"blaze check {scope}: " + ", ".join(parts) + "."
    if result["unchanged_errors"]:
        line += f" {plural(result['unchanged_errors'], 'error')} said before still there (`{result['rerun']}` shows all)."
    return line


def render(result: dict[str, Any], drawn_errors: list[str], drawn_warnings: list[str]) -> str:
    out = [
        section(result, "error", drawn_errors, result["more_errors"]),
        section(result, "warning", drawn_warnings, result["more_warnings"]),
        summary(result),
    ]
    if result["note"]:
        out.append(result["note"])
    return "\n".join(x for x in out if x)


# ---------------------------------------------------------------------------------------------
# State: the session's last check of each package.


def session_dir(state_dir: Path, session: str) -> Path:
    return state_dir / hashlib.sha256(session.encode()).hexdigest()[:24]


def state_path(sdir: Path, manifest: Path) -> Path:
    return sdir / (hashlib.sha256(str(manifest).encode()).hexdigest()[:24] + ".json")


def load_base(path: Path, manifest: Path) -> tuple[collections.Counter | None, str | None]:
    """The last check's identities, None for none; a note when a file was there and unreadable,
    so a restore never quietly starts empty (rule persisted-state-round-trips)."""
    try:
        raw = path.read_text(encoding="utf-8")
    except FileNotFoundError:
        return None, None
    except OSError as error:
        return None, f"the last check's state {path} could not be read ({error}): shown as a first check"
    try:
        doc = json.loads(raw)
    except json.JSONDecodeError as error:
        return None, f"the last check's state {path} is not JSON ({error}): shown as a first check"
    if not isinstance(doc, dict) or doc.get("format") != STATE_FORMAT:
        found = doc.get("format") if isinstance(doc, dict) else type(doc).__name__
        return None, f"the last check's state {path} is format {found!r}, this script reads {STATE_FORMAT}: shown as a first check"
    if doc.get("manifest") != str(manifest):
        return None, f"the last check's state {path} is of {doc.get('manifest')!r}, not {manifest}: shown as a first check"
    return collections.Counter({tuple(k): int(n) for k, n in doc.get("keys", [])}), None


def save_base(path: Path, manifest: Path, keys: collections.Counter) -> None:
    doc = {
        "format": STATE_FORMAT,
        "manifest": str(manifest),
        "keys": sorted([list(k), n] for k, n in keys.items()),
    }
    write_atomic(path, json.dumps(doc, sort_keys=True, separators=(",", ":")) + "\n")


def write_atomic(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=path.name + ".", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(text)
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, path)
    except BaseException:
        try:
            os.unlink(tmp)
        except FileNotFoundError:
            pass
        raise


class Locked:
    """The session's lock on one package's state, taken within a deadline: two writes in one
    turn run their checks one after the other, and the second diffs against the first."""

    def __init__(self, path: Path, deadline: float, clock: Callable[[], float], sleep: Callable[[float], None]):
        path.parent.mkdir(parents=True, exist_ok=True)
        self.fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
        while True:  # bounded: ends at `deadline` (rule bounded-progress)
            try:
                fcntl.flock(self.fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                self.held = True
                return
            except OSError as error:
                if error.errno not in (errno.EWOULDBLOCK, errno.EAGAIN):
                    raise
            if clock() >= deadline:
                self.held = False
                return
            sleep(0.05)

    def release(self) -> None:
        os.close(self.fd)


# ---------------------------------------------------------------------------------------------
# The driver.


@dataclasses.dataclass(frozen=True)
class Ran:
    exit_code: int | None  # None: not done by the deadline
    stdout: str
    stderr: str
    start_error: str | None


def run_blaze(blaze: str, args: list[str], cwd: Path, deadline_s: float, out_dir: Path) -> Ran:
    """`blaze args`, its output in files under `out_dir`; left running past the deadline."""
    out_dir.mkdir(parents=True, exist_ok=True)
    stamp = f"{os.getpid()}-{time.monotonic_ns()}"
    out_path, err_path = out_dir / f"check-{stamp}.out", out_dir / f"check-{stamp}.err"
    with open(out_path, "wb") as out, open(err_path, "wb") as err:
        try:
            child = subprocess.Popen(
                [blaze, *args], cwd=cwd, stdin=subprocess.DEVNULL, stdout=out, stderr=err, start_new_session=True
            )
        except OSError as error:
            return Ran(None, "", "", f"could not start `{blaze}`: {error}")
    try:
        code = child.wait(timeout=deadline_s)
    except subprocess.TimeoutExpired:
        return Ran(None, "", "", None)
    stdout = out_path.read_text(encoding="utf-8", errors="replace")
    stderr = err_path.read_text(encoding="utf-8", errors="replace")
    for p in (out_path, err_path):
        p.unlink()
    return Ran(code, stdout, stderr, None)


def default_state_dir() -> Path:
    return Path(tempfile.gettempdir()) / "blaze-new-diagnostics"


def run(inp: Input, clock: Callable[[], float] = time.monotonic, sleep: Callable[[float], None] = time.sleep) -> dict[str, Any]:
    started = clock()
    cwd = Path(inp.cwd)
    file = Path(inp.file) if os.path.isabs(inp.file) else cwd / inp.file
    file = Path(os.path.abspath(file))
    owner = owner_of(file)
    if owner is None:
        return empty_result("skipped", file, None)
    state_dir = Path(inp.state_dir) if inp.state_dir else default_state_dir()
    sdir = session_dir(state_dir, inp.session)
    blaze = inp.blaze if inp.blaze else shutil.which("blaze")
    if blaze is None or not (os.path.isfile(blaze) and os.access(blaze, os.X_OK)):
        pin = pins_blaze(owner.workspace_root)
        if pin is None:
            return empty_result("skipped", file, owner)
        marker = sdir / "no-blaze"
        if marker.exists():
            return empty_result("skipped", file, owner)
        write_atomic(marker, "said\n")
        result = empty_result("no_blaze", file, owner)
        wanted = f"`{inp.blaze}` (BLAZE_BIN) is not an executable file" if inp.blaze else "no `blaze` is on PATH and BLAZE_BIN is not set"
        result["note"] = (
            f"blaze did not check this edit: {wanted}, and this workspace is built with blaze ({pin}). "
            "Install blaze (docs/install.md) or set BLAZE_BIN; this is said once a session."
        )
        result["text"] = result["note"]
        return result
    deadline = started + inp.deadline_s
    lock = Locked(sdir / (state_path(sdir, owner.manifest).stem + ".lock"), deadline, clock, sleep)
    try:
        if not lock.held:
            result = empty_result("timeout", file, owner)
            result["note"] = (
                f"blaze check did not answer in {inp.deadline_s:g} s: another check of {owner.name()} in this "
                f"session is still running; run `{owner.rerun()}` to see it"
            )
            result["text"] = result["note"]
            return result
        ran = run_blaze(blaze, owner.args(), owner.manifest.parent, max(0.01, deadline - clock()), sdir / "out")
        if ran.start_error is not None:
            result = empty_result("blaze_failed", file, owner)
            result["blaze_failed"] = ran.start_error
            result["text"] = f"blaze itself failed, not the code: {ran.start_error}"
            return result
        if ran.exit_code is None:
            result = empty_result("timeout", file, owner)
            result["note"] = (
                f"blaze check did not answer in {inp.deadline_s:g} s: it is still running in the daemon; "
                f"run `{owner.rerun()}` to see it"
            )
            result["text"] = result["note"]
            return result
        answer = read_messages(ran.stdout, owner)
        failure = blaze_failure(ran.exit_code, answer, ran.stderr)
        if failure is not None:
            if owner.unparsed:
                failure = f"{failure}\n(and the manifest itself: {owner.unparsed})"
            result = empty_result("blaze_failed", file, owner)
            result["blaze_failed"] = failure
            result["text"] = f"blaze itself failed while checking {owner.name()}, not the code: {failure}"
            return result
        spath = state_path(sdir, owner.manifest)
        base, note = load_base(spath, owner.manifest)
        d = diff(list(answer.diags), base, file, owner.workspace_root)
        save_base(spath, owner.manifest, d["keys"])
        return checked_result(file, owner, d, cwd, note)
    finally:
        lock.release()


def output(result: dict[str, Any], fmt: str) -> str:
    if fmt == "text":
        return result["text"]
    public = {k: v for k, v in result.items() if k not in ("error_text", "warning_text", "summary")}
    return json.dumps(public, sort_keys=True)


def hook_answer(results: list[dict[str, Any]], unread: list[str] = ()) -> dict[str, Any] | None:
    """The `PostToolUse` hook answer of a write's results, in the shape Claude Code and Codex
    both read (plugins/claude/FORMAT-NOTES.md, plugins/codex/FORMAT-NOTES.md): new errors as a
    blocking `decision` whose `reason` the model sees next to the tool result, so it fixes them
    before going on; warnings, counts and notes as `hookSpecificOutput.additionalContext`; blaze
    failing, or blaze missing where the project pins it, also as a `systemMessage` the person
    sees. None: nothing to say (no Rust file was written)."""
    errors = [r["error_text"] for r in results if r.get("error_text")]
    context: list[str] = []
    person: list[str] = []
    for r in results:
        if r["status"] == "checked":
            context += [x for x in (r["warning_text"], r["summary"], r["note"]) if x]
        elif r["status"] in ("timeout", "no_blaze", "blaze_failed"):
            context.append(r["text"])
            if r["status"] != "timeout":
                person.append(r["text"].splitlines()[0])
    context += list(unread)
    if not errors and not context:
        return None
    answer: dict[str, Any] = {}
    if errors:
        answer["decision"] = "block"
        answer["reason"] = (
            "The edit was applied, and blaze check found new errors in it; fix them before going on.\n"
            + "\n".join(errors)
        )
    if context:
        answer["hookSpecificOutput"] = {"hookEventName": "PostToolUse", "additionalContext": "\n".join(context)}
    if person:
        answer["systemMessage"] = "\n".join(person)
    return answer


def hook_failed(what: str, error: BaseException) -> dict[str, Any]:
    """The hook's own failure, said to the model and the person (never a quiet pass)."""
    text = f"the blaze hook itself failed ({what}), so this edit was not checked: {type(error).__name__}: {error}"
    return {"hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": text}, "systemMessage": text}


def read_value(arg: str) -> Any:
    if arg == "-":
        return json.load(sys.stdin)
    if arg.lstrip().startswith(("{", "[")):
        return json.loads(arg)
    return json.loads(Path(arg).read_text(encoding="utf-8"))


def main(argv: list[str]) -> int:
    if argv == ["--selftest"]:
        return selftest()
    try:
        if len(argv) == 2 and argv[0] == "--text":
            inp = Input.parse(
                {
                    "file": argv[1],
                    "cwd": os.getcwd(),
                    "session": "shell",
                    "deadline_s": 120,
                    "blaze": os.environ.get("BLAZE_BIN") or None,
                    "format": "text",
                }
            )
        elif len(argv) == 1:
            inp = Input.parse(read_value(argv[0]))
        else:
            raise InputError("usage: blaze_new_diagnostics.py '<json>' | - | --text FILE | --selftest")
    except (InputError, json.JSONDecodeError, OSError) as error:
        print(f"blaze-new-diagnostics: {error}", file=sys.stderr)
        return 2
    result = run(inp)
    text = output(result, inp.format)
    if text:
        print(text)
    if inp.format == "text":
        failing = result["errors"] or result["unchanged_errors"] or result["elsewhere"]["errors"]
        return 1 if failing or result["status"] in ("blaze_failed", "timeout", "no_blaze") else 0
    return 0


# ---------------------------------------------------------------------------------------------
# Selftest: a fixture project, a fake blaze, the expected answers.


def fake_blaze(path: Path, stdout: str, stderr: str, code: int, sleep_s: float, argv_log: Path) -> None:
    script = f"""#!{sys.executable} -I
import json, sys, time
with open({str(argv_log)!r}, "a") as f:
    f.write(json.dumps(sys.argv[1:]) + "\\n")
time.sleep({sleep_s!r})
sys.stdout.write({stdout!r})
sys.stderr.write({stderr!r})
sys.exit({code!r})
"""
    path.write_text(script)
    path.chmod(0o755)


def message(manifest: Path, level: str, code: str | None, text: str, file: str, line: int, col: int, source: str, helps: Iterable[str] = (), label: str | None = None) -> str:
    return json.dumps(
        {
            "reason": "compiler-message",
            "package_id": f"path+file://{manifest.parent}#demo@0.1.0",
            "manifest_path": str(manifest),
            "target": {"kind": ["lib"], "name": "demo"},
            "message": {
                "$message_type": "diagnostic",
                "message": text,
                "code": {"code": code, "explanation": None} if code else None,
                "level": level,
                "spans": [
                    {
                        "file_name": file,
                        "line_start": line,
                        "line_end": line,
                        "column_start": col,
                        "column_end": col + 1,
                        "is_primary": True,
                        "text": [{"text": source, "highlight_start": col, "highlight_end": col + 1}],
                        "label": label,
                    }
                ],
                "children": [{"message": h, "level": "help", "spans": [], "children": []} for h in helps],
                "rendered": text,
            },
        }
    )


def finished(success: bool) -> str:
    return json.dumps({"reason": "build-finished", "success": success, "served": "daemon", "took_ms": 42})


def selftest() -> int:
    failures: list[str] = []
    names: list[str] = []

    def expect(name: str, got: Any, want: Any) -> None:
        names.append(name)
        if got != want:
            failures.append(f"{name}: got {got!r}, want {want!r}")

    with tempfile.TemporaryDirectory(prefix="blaze-new-diagnostics-selftest-") as tmp:
        root = Path(os.path.realpath(tmp))
        proj = root / "demo"
        (proj / "src").mkdir(parents=True)
        manifest = proj / "Cargo.toml"
        manifest.write_text('[package]\nname = "demo"\nversion = "0.1.0"\nedition = "2024"\n')
        (proj / "src" / "lib.rs").write_text("pub fn a() {}\nfn unused() {}\nfn x() { let y = 1; }\n")
        (proj / "src" / "other.rs").write_text("fn z() {}\n")
        state = root / "state"
        fake = root / "bin" / "blaze"
        fake.parent.mkdir()
        argv_log = root / "argv.log"

        def scenario(stdout_lines: list[str], *, code: int = 0, stderr: str = "", sleep_s: float = 0.0, deadline: float = 10.0, session: str = "s1", file: str = "src/lib.rs", blaze: str | None = None) -> dict[str, Any]:
            fake_blaze(fake, "".join(l + "\n" for l in stdout_lines), stderr, code, sleep_s, argv_log)
            return run(Input(file=file, cwd=str(proj), session=session, deadline_s=deadline, state_dir=str(state), blaze=blaze or str(fake)))

        unused = message(manifest, "warning", "dead_code", "function `unused` is never used", "src/lib.rs", 2, 4, "fn unused() {}", ["remove it"])
        y = message(manifest, "warning", "unused_variables", "unused variable: `y`", "src/lib.rs", 3, 14, "fn x() { let y = 1; }", ["if this is intentional, prefix it with an underscore: `_y`"])
        z = message(manifest, "warning", "dead_code", "function `z` is never used", "src/other.rs", 1, 4, "fn z() {}")
        mismatch = message(manifest, "error", "E0308", "mismatched types", "src/lib.rs", 1, 14, "pub fn a() { 1 }", label="expected `()`, found integer")

        # 1. First run: the edited file's two warnings drawn, the other file's one counted.
        r = scenario([unused, y, z, finished(True)])
        expect("first.status", r["status"], "checked")
        expect("first.first_run", r["first_run"], True)
        expect("first.warnings", [w["code"] for w in r["warnings"]], ["dead_code", "unused_variables"])
        expect("first.elsewhere", r["elsewhere"], {"errors": 0, "warnings": 1})
        expect("first.text.line", r["text"].splitlines()[1], "src/lib.rs:2:4: warning[dead_code]: function `unused` is never used")
        expect("first.text.help", r["text"].splitlines()[2], "  help: remove it")
        argv = json.loads(argv_log.read_text().splitlines()[-1])
        expect("first.argv", argv, ["check", "--manifest-path", str(manifest), "-p", "demo", "--all-targets", "--message-format", "json", "--color", "never"])

        # 2. A new error: drawn; the three warnings unchanged.
        r = scenario([mismatch, unused, y, z, finished(False)], code=101)
        expect("error.status", r["status"], "checked")
        expect("error.errors", [(e["code"], e["line"]) for e in r["errors"]], [("E0308", 1)])
        expect("error.warnings", r["warnings"], [])
        expect("error.counts", (r["fixed"], r["unchanged"]), (0, 3))
        expect("error.text", r["text"].splitlines()[:2], ["blaze check -p demo: 1 new error in demo:", "src/lib.rs:1:14: error[E0308]: mismatched types"])

        # 2b. The error still there on the next check: not new, but said as still there.
        r = scenario([mismatch, unused, y, z, finished(False)], code=101)
        expect("still.counts", (len(r["errors"]), r["unchanged"], r["unchanged_errors"]), (0, 4, 1))
        expect("still.text", "1 error said before still there" in r["text"], True)

        # 3. A fixed warning (and the error fixed): nothing new, two fixed, two unchanged.
        r = scenario([unused, z, finished(True)])
        expect("fixed.new", (r["errors"], r["warnings"]), ([], []))
        expect("fixed.counts", (r["fixed"], r["unchanged"]), (2, 2))
        expect("fixed.text", r["text"], "blaze check -p demo: nothing new, 2 fixed, 2 unchanged.")

        # 4. A moved line: three lines inserted above, every diagnostic three lines down, and a
        # message that names a line: the same diagnostics, so nothing new and nothing fixed.
        moved_unused = message(manifest, "warning", "dead_code", "function `unused` is never used", "src/lib.rs", 5, 4, "fn unused() {}", ["remove it"])
        moved_z = message(manifest, "warning", "dead_code", "function `z` is never used", "src/other.rs", 4, 4, "fn z() {}")
        r = scenario([moved_unused, moved_z, finished(True)])
        expect("moved.new", (r["errors"], r["warnings"]), ([], []))
        expect("moved.counts", (r["fixed"], r["unchanged"]), (0, 2))
        expect("normalize", normalize("first defined at src/lib.rs:12:5, line 12"), "first defined at src/lib.rs#, #")

        # 5. A timeout: said, and the base kept (the next run still diffs against run 4).
        t0 = time.monotonic()
        r = scenario([moved_unused, finished(True)], sleep_s=3.0, deadline=1.0)
        expect("timeout.status", r["status"], "timeout")
        expect("timeout.text", r["text"], "blaze check did not answer in 1 s: it is still running in the daemon; run `blaze check -p demo --all-targets` to see it")
        expect("timeout.bounded", time.monotonic() - t0 < 2.5, True)
        r = scenario([moved_unused, moved_z, finished(True)])
        expect("after-timeout.counts", (r["fixed"], r["unchanged"], len(r["warnings"])), (0, 2, 0))

        # 6. blaze missing: quiet where nothing pins blaze; said once a session where blaze.lock does.
        missing = str(root / "bin" / "no-such-blaze")
        r = scenario([], blaze=missing, session="s2")
        expect("missing.unpinned", (r["status"], r["text"]), ("skipped", ""))
        (proj / "blaze.lock").write_text("version = 1\n")
        r = scenario([], blaze=missing, session="s2")
        expect("missing.pinned.status", r["status"], "no_blaze")
        expect("missing.pinned.names", "blaze.lock" in r["text"] and "BLAZE_BIN" in r["text"], True)
        r = scenario([], blaze=missing, session="s2")
        expect("missing.once", r["status"], "skipped")
        (proj / "blaze.lock").unlink()

        # 7. blaze crashing: its last lines, said as blaze failing; the base kept.
        r = scenario([], code=101, stderr="thread 'main' panicked at crates/blaze-cargo/src/serve.rs:10:5:\nthe daemon went away\n")
        expect("crash.status", r["status"], "blaze_failed")
        expect("crash.names", "panicked" in r["blaze_failed"] and "build-finished" in r["blaze_failed"], True)
        expect("crash.text", r["text"].startswith("blaze itself failed while checking demo, not the code"), True)
        r = scenario([finished(False)], code=101, stderr="blaze: failed errors=0\n")
        expect("failed-no-error.status", r["status"], "blaze_failed")
        r = scenario([moved_unused, moved_z, finished(True)])
        expect("after-crash.counts", (r["fixed"], r["unchanged"]), (0, 2))

        # 8. Out of scope: not Rust, or no Cargo.toml above.
        (proj / "README.md").write_text("x\n")
        r = scenario([], file="README.md")
        expect("not-rust", r["status"], "skipped")
        (root / "loose.rs").write_text("fn main() {}\n")
        r = scenario([], file=str(root / "loose.rs"))
        expect("no-manifest", r["status"], "skipped")

        # 9. The cap: 25 new warnings drawn as 10 and "and 15 more".
        many = [message(manifest, "warning", "dead_code", f"function `f{i}` is never used", "src/lib.rs", 10 + i, 4, f"fn f{i}() {{}}") for i in range(25)]
        r = scenario(many + [finished(True)], session="s3")
        expect("cap.drawn", (len(r["warnings"]), r["more_warnings"]), (10, 15))
        expect("cap.text", "and 15 more: run `blaze check -p demo --all-targets`" in r["text"], True)

        # 10. A diagnostic said for two targets (lib and its unit tests) is one.
        r = scenario([unused, unused, finished(True)], session="s4")
        expect("dedupe", len(r["warnings"]), 1)

        # 11. A state file of another layout is said, not silently dropped.
        sp = state_path(session_dir(state, "s4"), manifest)
        sp.write_text(json.dumps({"format": 0, "keys": []}))
        r = scenario([unused, finished(True)], session="s4")
        expect("old-state.note", "format 0" in (r["note"] or ""), True)
        expect("old-state.first", r["first_run"], True)

        # 12. The input refuses fields it does not know.
        try:
            Input.parse({"file": "a.rs", "cwd": "/", "session": "s", "deadline_s": 1, "colour": 1})
            failures.append("input: an unknown field was accepted")
        except InputError as error:
            expect("input.names", "colour" in str(error), True)

    for f in failures:
        print(f"FAIL {f}")
    scenarios = sorted({n.split(".")[0] for n in names})
    print(f"blaze-new-diagnostics selftest: {'ok' if not failures else 'FAILED'}: {len(names) - len(failures)}/{len(names)} checks, scenarios {', '.join(scenarios)}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
