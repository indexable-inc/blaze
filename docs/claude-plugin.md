---
status: current
summary: blaze's plugin for coding agents: after every write of a Rust file, Claude Code or Codex is told the errors and warnings `blaze check` has that are new since the last check of that package; how to install it, what the hook prints, and the launcher for agents without hooks.
---
# blaze for coding agents

An agent that edits Rust should hear about the error it just made at the write that made it, not
at the next build it thinks to run. The plugin runs `blaze check` of the package that owns each
written file and gives the agent only what changed: new errors, new warnings, and how many were
fixed and how many are unchanged. The daemon makes the repeat check cheap, which is what makes a
check per write affordable.

One core, `plugins/agent/blaze_new_diagnostics.py`, does the work for every agent; thin adapters
map each agent's hook input and output to it:

| agent | how it is wired | files |
|---|---|---|
| Claude Code | plugin with a `PostToolUse` hook on `Write\|Edit`, and two commands | `plugins/.claude-plugin/plugin.json`, `plugins/claude/` |
| Codex | `PostToolUse` hook on `apply_patch` in `hooks.json` | `plugins/codex/check.py` |
| any other | `blaze-new-diagnostics FILE` on `PATH`, and a line in its instructions | `plugins/agent/` |

## Install

Claude Code, in a session:

    /plugin marketplace add indexable-inc/blaze
    /plugin install blaze@blaze

Without plugins, or for the other agents, `plugins/claude/install.py` takes one JSON value and
prints the change as a diff; `"write": true` applies it, merged into what the file holds:

    python3 -I plugins/claude/install.py '{"agent": "claude", "scope": "project"}'   .claude/settings.json
    python3 -I plugins/claude/install.py '{"agent": "codex", "scope": "user"}'       ~/.codex/hooks.json
    python3 -I plugins/claude/install.py '{"agent": "generic", "scope": "user"}'     ~/.local/bin/blaze-new-diagnostics

Codex runs a new hook only after it is trusted in `/hooks`. Every form needs a `blaze` on `PATH`
(or `BLAZE_BIN`) and Python 3.11 or later.

## What the hook does

For a `.rs` file or a `Cargo.toml` with a `Cargo.toml` at or above it, it runs

    blaze check --manifest-path <the package's Cargo.toml> -p <package> --all-targets --message-format json

and reads the `compiler-message` lines and the closing `build-finished` ([cargo.md](cargo.md)).
It keeps the session's last check of each package and compares by identity: level, code, file,
the message with line references normalized, the source line the diagnostic points at and its
label. Not the line number: an inserted line would make every diagnostic below it read as fixed
and new again.

What the agent sees:

```text
The edit was applied, and blaze check found new errors in it; fix them before going on.
blaze check -p demo: 1 new error in demo:
src/lib.rs:1:14: error[E0308]: mismatched types
  help: remove the trailing value
blaze check -p demo: 1 new, 0 fixed, 3 unchanged.
```

- new errors block (the agent reads them next to the tool result and fixes them first); new
  warnings and the line of counts are context; an error said before and still there is counted
  in that line;
- the first check of a package in a session shows the written file's diagnostics and counts the
  rest of the package;
- at most 10 of each level, then `and N more: run blaze check -p <pkg> --all-targets`;
- past 20 s: `blaze check did not answer in 20 s: it is still running in the daemon`;
- blaze failing (it will not start, panics, dies, refuses its command line, ends without
  `build-finished`, or fails naming no error): its last lines, said as blaze failing, not the
  code, to the agent and to the person;
- no blaze: nothing, unless the workspace pins blaze (`blaze.lock` or
  `[workspace.metadata.blaze]`); then once a session that its edits go unchecked.

The hook formats each agent reads, with the documentation lines they rest on, are in
`plugins/claude/FORMAT-NOTES.md` and `plugins/codex/FORMAT-NOTES.md`. Each script has a
`--selftest` that runs fixtures against a fake blaze.
