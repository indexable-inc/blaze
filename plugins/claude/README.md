# blaze for Claude Code

After every `Write` or `Edit` of a `.rs` file or a `Cargo.toml`, the plugin runs `blaze check` of
the package that owns the file and tells Claude what is new since the last check of that package
in the session:

- new errors block (Claude Code's `decision: "block"`), with each as
  `path:line:col: error[code]: message` and blaze's help lines, so Claude fixes them before it
  goes on;
- new warnings, and one line of counts (`nothing new, 2 fixed, 5 unchanged`), as context;
- the first check of a package in a session shows the edited file's diagnostics and counts the
  rest; at most 10 of each level are drawn, then `and N more: run blaze check -p <pkg> --all-targets`;
- when blaze itself fails (it will not start, panics, refuses its command line), blaze's last
  lines, said as blaze failing, not the code; when the check takes over 20 s, that it is still
  running in the daemon.

It also adds `/blaze:blaze-check` (every diagnostic of the workspace) and `/blaze:blaze-why`
(why the last build did what it did, from `blaze why` and `blaze slow`).

## Install

    /plugin marketplace add indexable-inc/blaze
    /plugin install blaze@blaze

It needs a `blaze` on `PATH`, or `BLAZE_BIN` naming one, and Python 3.11 or later as `python3`.
Without a blaze it does nothing, except in a workspace that pins blaze (`blaze.lock` or
`[workspace.metadata.blaze]`), where it says once a session that the edits go unchecked.

Without plugins: `python3 -I plugins/claude/install.py '{"scope": "project"}'` prints the change
to `.claude/settings.json` as a diff; add `"write": true` to apply it (`"scope": "user"` for
`~/.claude/settings.json`). The same script installs the Codex hook and the launcher for other
agents (`"agent": "codex"`, `"agent": "generic"`; plugins/agent/README.md).

## Turn it off

`/plugin disable blaze@blaze` (or `claude plugin disable blaze@blaze` from a shell). An install by
install.py is removed by deleting its handler from the settings file.

## Cost

One `blaze check -p <pkg> --all-targets` per write of a Rust file: tens of milliseconds against a
warm daemon, the whole first check of the package when it is cold (the hook waits at most 20 s,
then lets it finish in the background). Each answer adds a few lines to Claude's context.

Formats and the doc lines they follow: FORMAT-NOTES.md. The core: plugins/agent/blaze_new_diagnostics.py.
