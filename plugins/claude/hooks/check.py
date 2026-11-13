#!/usr/bin/env python3
"""The Claude Code adapter of blaze-new-diagnostics: a `PostToolUse` hook on `Write|Edit`.

    python3 -I check.py              the hook: Claude Code's hook JSON on stdin
    python3 -I check.py --selftest   the mapping, on fixture hook input and fixture results

Claude Code sends one JSON object on stdin (plugins/claude/FORMAT-NOTES.md): `session_id`,
`cwd`, `tool_name`, `tool_input.file_path` (absolute for Write and Edit), and, from v2.1.257,
`scratchpad_dir`, the session's temporary directory. This script maps that to the core's input
(plugins/agent/blaze_new_diagnostics.py), runs it, and maps the core's result to the hook answer
Claude reads (`blaze_new_diagnostics.hook_answer`): new errors block with the diagnostics as the
reason, the rest is additional context.

This one script reads the hook's JSON leniently, against the rule that a script's input refuses
fields it does not know (scripts-take-typed-input): the input is not ours. Claude Code adds
fields to it release by release (`prompt_id`, `effort`, `scratchpad_dir` all came later), and a
hook that refused them would stop checking edits on the next update. It takes the fields it
reads, typed, and ignores the rest. The core it calls refuses unknown fields as the rule says.

The deadline is DEADLINE_S, under the hook's `timeout` in hooks.json (30 s), so the core says
"still running" itself rather than Claude Code cancelling the hook and discarding its output.
"""

from __future__ import annotations

import dataclasses
import importlib.util
import json
import sys
import time
from pathlib import Path
from typing import Any

DEADLINE_S = 20.0
CORE = Path(__file__).resolve().parents[2] / "agent" / "blaze_new_diagnostics.py"


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
    file_path: str | None
    scratchpad_dir: str | None

    @staticmethod
    def parse(value: Any) -> "HookInput":
        if not isinstance(value, dict):
            raise ValueError(f"the hook input is a JSON object, not {type(value).__name__}")
        tool_input = value.get("tool_input") if isinstance(value.get("tool_input"), dict) else {}

        def text(v: Any) -> str | None:
            return v if isinstance(v, str) and v else None

        session, cwd = text(value.get("session_id")), text(value.get("cwd"))
        if session is None or cwd is None:
            raise ValueError("the hook input has no `session_id` or `cwd`")
        return HookInput(
            session_id=session,
            cwd=cwd,
            tool_name=text(value.get("tool_name")) or "",
            file_path=text(tool_input.get("file_path")),
            scratchpad_dir=text(value.get("scratchpad_dir")),
        )


def core_input(hook: HookInput, blaze: str | None, deadline_s: float) -> dict[str, Any] | None:
    if hook.file_path is None:
        return None
    return {
        "file": hook.file_path,
        "cwd": hook.cwd,
        "session": hook.session_id,
        "deadline_s": deadline_s,
        "state_dir": str(Path(hook.scratchpad_dir) / "blaze-new-diagnostics") if hook.scratchpad_dir else None,
        "blaze": blaze,
    }


def main() -> int:
    import os

    try:
        core = load_core()
    except Exception as error:  # the plugin is broken: say it, never pass quietly
        print(json.dumps({"systemMessage": f"the blaze hook could not load its core {CORE}: {error}"}))
        return 0
    try:
        hook = HookInput.parse(json.load(sys.stdin))
        inp = core_input(hook, os.environ.get("BLAZE_BIN") or None, DEADLINE_S)
        if inp is None:
            return 0
        result = core.run(core.Input.parse(inp))
        answer = core.hook_answer([result])
    except Exception as error:
        answer = core.hook_failed("plugins/claude/hooks/check.py", error)
    if answer is not None:
        print(json.dumps(answer))
    return 0


def selftest() -> int:
    core = load_core()
    failures: list[str] = []

    def expect(name: str, got: Any, want: Any) -> None:
        if got != want:
            failures.append(f"{name}: got {got!r}, want {want!r}")

    stdin = {
        "session_id": "abc123",
        "transcript_path": "/x.jsonl",
        "cwd": "/w",
        "permission_mode": "default",
        "hook_event_name": "PostToolUse",
        "tool_name": "Write",
        "tool_input": {"file_path": "/w/src/lib.rs", "content": "fn main() {}"},
        "tool_response": {"filePath": "/w/src/lib.rs", "type": "create"},
        "tool_use_id": "toolu_01",
        "duration_ms": 12,
        "a_field_from_a_later_release": {"x": 1},
        "scratchpad_dir": "/tmp/claude/s/scratchpad",
    }
    hook = HookInput.parse(stdin)
    inp = core_input(hook, None, DEADLINE_S)
    expect("input", inp, {
        "file": "/w/src/lib.rs", "cwd": "/w", "session": "abc123", "deadline_s": 20.0,
        "state_dir": "/tmp/claude/s/scratchpad/blaze-new-diagnostics", "blaze": None,
    })
    core.Input.parse(inp)  # the core takes it
    expect("no-file", core_input(HookInput.parse({"session_id": "s", "cwd": "/", "tool_input": {}}), None, 1), None)

    def result(**kw: Any) -> dict[str, Any]:
        r = core.empty_result(kw.pop("status", "checked"), Path("/w/src/lib.rs"), None)
        r.update({"error_text": "", "warning_text": "", "summary": "", **kw})
        return r

    a = core.hook_answer([result(error_text="blaze check -p demo: 1 new error in demo:\nsrc/lib.rs:1:14: error[E0308]: mismatched types", warning_text="blaze check -p demo: 1 new warning in demo:\nsrc/lib.rs:2:4: warning[dead_code]: function `unused` is never used", summary="blaze check -p demo: 2 new, 0 fixed, 3 unchanged.")])
    expect("error.decision", a["decision"], "block")
    expect("error.reason", a["reason"].splitlines()[2], "src/lib.rs:1:14: error[E0308]: mismatched types")
    expect("error.context", a["hookSpecificOutput"]["additionalContext"].splitlines()[-1], "blaze check -p demo: 2 new, 0 fixed, 3 unchanged.")
    expect("error.event", a["hookSpecificOutput"]["hookEventName"], "PostToolUse")
    a = core.hook_answer([result(summary="blaze check -p demo: nothing new, 1 fixed, 2 unchanged.")])
    expect("clean", a, {"hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": "blaze check -p demo: nothing new, 1 fixed, 2 unchanged."}})
    a = core.hook_answer([result(status="blaze_failed", text="blaze itself failed while checking demo, not the code: blaze exited 101\nthread 'main' panicked")])
    expect("failed.no-block", "decision" in a, False)
    expect("failed.person", a["systemMessage"], "blaze itself failed while checking demo, not the code: blaze exited 101")
    a = core.hook_answer([result(status="timeout", text="blaze check did not answer in 20 s: it is still running in the daemon; run `blaze check -p demo --all-targets` to see it")])
    expect("timeout", ("decision" in a, "systemMessage" in a), (False, False))
    expect("skipped", core.hook_answer([result(status="skipped")]), None)
    a = core.hook_failed("x", ValueError("boom"))
    expect("own-failure", "boom" in a["systemMessage"] and "not checked" in a["hookSpecificOutput"]["additionalContext"], True)

    for f in failures:
        print(f"FAIL {f}")
    print(f"claude hook selftest: {'ok' if not failures else 'FAILED'} ({len(failures)} failure(s))")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(selftest() if sys.argv[1:] == ["--selftest"] else main())
