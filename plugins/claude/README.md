# blaze for Claude Code

After every `Write` or `Edit` of a `.rs` file or a `Cargo.toml`, the plugin asks
blaze's daemon what a check of the package that owns the file says that is new since the last
check of that package in the session, and tells Claude:

- new errors block (Claude Code's `decision: "block"`), each as
  `path:line:col: error[code]: message` with its help lines, so Claude fixes them before it goes
  on;
- new warnings, and one line of counts (`nothing new, 2 fixed, 5 unchanged`), as context;
- the first check of a package in a session shows the edited file's diagnostics and counts the
  rest; at most 10 of each level are drawn, then `and N more: run blaze diagnostics -p <pkg>`;
- when blaze itself fails, that it is blaze failing, not the code; when the check takes over
  20 s, that it is still running in the daemon.

Before every `Bash` command it looks for `cargo build|check|test|run|clippy|bench|doc` and `rustc`,
and tells Claude to run blaze's instead.

It also adds `/blaze:blaze-check` (every diagnostic of the workspace) and `/blaze:blaze-why`
(why the last build did what it did, from `blaze why` and `blaze slow`).

Both hooks run `blaze agents hook claude` (crates/blaze-cargo/src/cli/agents.rs), so they need a
`blaze` on the `PATH` Claude Code runs with.

## Install

    /plugin marketplace add indexable-inc/blaze
    /plugin install blaze@blaze

Without the plugin, `blaze agents install claude` writes the same hooks into
`.claude/settings.json` (or `~/.claude/settings.json` with `--user`), with a block in `CLAUDE.md`
that says to build with blaze; `--strict` makes the `Bash` hook refuse cargo and rustc. Use one or
the other, not both: each runs the check. docs/agents.md says all of it.

## Turn it off

`/plugin disable blaze@blaze` (or `claude plugin disable blaze@blaze` from a shell);
`blaze agents remove claude` for an install by `blaze agents install`.

## Cost

One `Diagnostics` request per write of a Rust file: tens of milliseconds against a warm daemon, the
whole first check of the package when it is cold (the hook waits at most 20 s, then lets it finish
in the daemon). Each answer adds a few lines to Claude's context.

Formats and the doc lines they follow: FORMAT-NOTES.md.
