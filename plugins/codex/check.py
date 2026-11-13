#!/usr/bin/env python3
"""The Codex adapter of blaze-new-diagnostics: a `PostToolUse` hook on `apply_patch`.

    python3 -I check.py              the hook: Codex's hook JSON on stdin
    python3 -I check.py --selftest   the mapping, on fixture hook input

Codex sends one JSON object on stdin (plugins/codex/FORMAT-NOTES.md): `session_id`, `cwd`,
`tool_name` (`apply_patch` for file edits), and `tool_input.command`, which for `apply_patch` is
the patch. The files a patch writes are read from its `*** Add File:`, `*** Update File:`,
`*** Delete File:` and `*** Move to:` lines (the patch format is not on the hooks page: an
assumption, named in FORMAT-NOTES.md). A patch in which none is found is said, not passed.
Each file is checked by the core (plugins/agent/blaze_new_diagnostics.py) under one deadline,
and the answer is the shape Codex reads (`blaze_new_diagnostics.hook_answer`), the same one
Claude Code reads: Codex replaces the tool result with a blocking `reason`, so the reason says
first that the patch was applied.

The hook's JSON is read leniently, as plugins/claude/hooks/check.py says why: it is Codex's, and
Codex adds fields to it. A `tool_input.file_path` is taken too, for a tool that sends one.
"""

from __future__ import annotations

import dataclasses
import importlib.util
import json
import os
import re
import sys
import time
from pathlib import Path
from typing import Any

DEADLINE_S = 20.0
CORE = Path(__file__).resolve().parents[1] / "agent" / "blaze_new_diagnostics.py"
PATCH_FILE = re.compile(r"^\*\*\* (?:Add|Update|Delete) File: (.+?)\s*$|^\*\*\* Move to: (.+?)\s*$", re.M)


def load_core():
    sys.dont_write_bytecode = True  # no __pycache__ in the checkout or the plugin cache
    spec = importlib.util.spec_from_file_location("blaze_new_diagnostics", CORE)
    if spec is None or spec.loader is None:
        raise ImportError(f"the core is not at {CORE}")
    module = importlib.util.module_from_spec(spec)
    sys.modules["blaze_new_diagnostics"] = module
    spec.loader.exec_module(module)
    return module


@dataclasses.dataclass(frozen=True)
class HookInput:
    session_id: str
    cwd: str
    tool_name: str
    files: tuple[str, ...]
    unread: str | None  # a write whose files could not be read from its input

    @staticmethod
    def parse(value: Any) -> "HookInput":
        if not isinstance(value, dict):
            raise ValueError(f"the hook input is a JSON object, not {type(value).__name__}")
        session, cwd = value.get("session_id"), value.get("cwd")
        if not isinstance(session, str) or not session or not isinstance(cwd, str) or not cwd:
            raise ValueError("the hook input has no `session_id` or `cwd`")
        tool = value.get("tool_name") if isinstance(value.get("tool_name"), str) else ""
        tool_input = value.get("tool_input")
        files: list[str] = []
        unread = None
        if isinstance(tool_input, dict) and isinstance(tool_input.get("file_path"), str):
            files.append(tool_input["file_path"])
        elif isinstance(tool_input, dict) and isinstance(tool_input.get("command"), str):
            for m in PATCH_FILE.finditer(tool_input["command"]):
                files.append(m.group(1) or m.group(2))
            if not files:
                unread = f"the blaze hook found no `*** ... File:` line in this {tool or 'tool'} input, so it checked nothing"
        else:
            unread = f"the blaze hook could not read which files this {tool or 'tool'} call wrote (no `tool_input.command` or `file_path`), so it checked nothing"
        seen: list[str] = []
        for f in files:
            if f not in seen:
                seen.append(f)
        return HookInput(session, cwd, tool, tuple(seen), unread)


def core_inputs(hook: HookInput, blaze: str | None, deadline_s: float) -> list[dict[str, Any]]:
    return [
        {"file": f, "cwd": hook.cwd, "session": hook.session_id, "deadline_s": deadline_s, "blaze": blaze}
        for f in hook.files
    ]


def main() -> int:
    try:
        core = load_core()
    except Exception as error:
        print(json.dumps({"systemMessage": f"the blaze hook could not load its core {CORE}: {error}"}))
        return 0
    try:
        hook = HookInput.parse(json.load(sys.stdin))
        end = time.monotonic() + DEADLINE_S
        results = []
        for inp in core_inputs(hook, os.environ.get("BLAZE_BIN") or None, DEADLINE_S):
            left = end - time.monotonic()
            if left <= 0:
                break  # the last result already says the check is still running
            results.append(core.run(core.Input.parse({**inp, "deadline_s": left})))
        answer = core.hook_answer(results, [hook.unread] if hook.unread else [])
        if answer is not None and answer.get("reason"):
            answer["reason"] = answer["reason"].replace("The edit was applied", "The patch was applied", 1)
    except Exception as error:
        answer = core.hook_failed("plugins/codex/check.py", error)
    if answer is not None:
        print(json.dumps(answer))
    return 0


def selftest() -> int:
    failures: list[str] = []

    def expect(name: str, got: Any, want: Any) -> None:
        if got != want:
            failures.append(f"{name}: got {got!r}, want {want!r}")

    patch = (
        "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-fn a() {}\n+fn a() { 1 }\n"
        "*** Add File: src/new.rs\n+pub fn n() {}\n*** Update File: src/old.rs\n*** Move to: src/moved.rs\n"
        "*** Update File: src/lib.rs\n*** End Patch\n"
    )
    stdin = {
        "session_id": "019a-codex", "transcript_path": None, "cwd": "/w", "hook_event_name": "PostToolUse",
        "model": "gpt-6.1", "turn_id": "t1", "permission_mode": "default", "tool_name": "apply_patch",
        "tool_use_id": "call_1", "tool_input": {"command": patch}, "tool_response": "Success.",
    }
    hook = HookInput.parse(stdin)
    expect("files", hook.files, ("src/lib.rs", "src/new.rs", "src/old.rs", "src/moved.rs"))
    expect("inputs", core_inputs(hook, None, 20.0)[0], {"file": "src/lib.rs", "cwd": "/w", "session": "019a-codex", "deadline_s": 20.0, "blaze": None})
    heredoc = HookInput.parse({**stdin, "tool_input": {"command": "apply_patch <<'EOF'\n" + patch + "EOF\n"}})
    expect("heredoc", heredoc.files[0], "src/lib.rs")
    none = HookInput.parse({**stdin, "tool_input": {"command": "echo hi"}})
    expect("unread", (none.files, none.unread is not None), ((), True))
    odd = HookInput.parse({**stdin, "tool_input": ["x"]})
    expect("odd-input", odd.unread is not None, True)
    load_core().Input.parse(core_inputs(hook, None, 20.0)[0])

    for f in failures:
        print(f"FAIL {f}")
    print(f"codex hook selftest: {'ok' if not failures else 'FAILED'} ({len(failures)} failure(s))")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(selftest() if sys.argv[1:] == ["--selftest"] else main())
