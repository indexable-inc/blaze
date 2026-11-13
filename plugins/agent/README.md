# blaze-new-diagnostics, for any coding agent

`blaze_new_diagnostics.py` is the agent-neutral core of blaze's agent plugins: given the file an
agent just wrote, it runs `blaze check` of the package that owns it and returns only the errors
and warnings that are new since the last check of that package, with counts of what was fixed
and what is unchanged. Claude Code (plugins/claude) and Codex (plugins/codex) call it from their
after-edit hooks; any other agent calls the launcher.

## For an agent without hooks

Link the launcher onto `PATH`:

    python3 -I plugins/claude/install.py '{"agent": "generic", "scope": "user", "write": true}'

and put this one line in the agent's instructions file (AGENTS.md, or what it reads):

    After you edit a Rust file or a Cargo.toml, run `blaze-new-diagnostics <file>` and fix the errors it prints before going on; it prints only what is new since its last run.

`blaze-new-diagnostics FILE` prints `path:line:col: level[code]: message` lines with blaze's help
lines, then one line of counts; it exits 1 while the package has an error (new, or said before
and still there), or when blaze failed, timed out (120 s) or is missing where the project pins
it, else 0. It needs `blaze` on `PATH` (or `BLAZE_BIN`) and Python 3.11 or later.

## The core's interface

One JSON value in, one out (the module docstring has every field):

    python3 -I blaze_new_diagnostics.py '{"file": "src/lib.rs", "cwd": "/w", "session": "s1", "deadline_s": 20}'
    python3 -I blaze_new_diagnostics.py --selftest
