---
status: current
summary: blaze for coding agents: `blaze agents install|status|remove` wires Claude Code and Codex to blaze (a hook after each edit that says the new errors and warnings, a hook before each shell command that steers cargo and rustc to blaze, and a block of instructions); `blaze agents hook` is that hook; `blaze diagnostics --since` is the request under it, for any agent; `blaze doctor` says whether all of it is installed and current.
---
# blaze for coding agents

An agent that edits Rust should hear about the error it just made at the write that made it, not
at the next build it thinks to run, and should build with blaze, not cargo. blaze does both
itself; there is no script and no Python:

| command | what it does |
|---|---|
| `blaze agents install [claude\|codex]... [--project\|--user] [--strict] [--dry-run]` | writes the hooks and the instructions block into the agents' files |
| `blaze agents status [--json]` | says, for each agent and scope, what is installed and whether it is current |
| `blaze agents remove [claude\|codex]... [--project\|--user] [--dry-run]` | takes out blaze's entries and block, nothing else |
| `blaze agents hook <claude\|codex> v<N> [--strict]` | the hook the agent runs; reads its JSON on stdin |
| `blaze diagnostics [--since TOKEN] [cargo's package flags] [--file F] [--client NAME] [--wait S] [--json]` | what a check says that is new since `TOKEN` |

`blaze doctor` reports the agents' installs in its `agents` section ([cargo.md](cargo.md#blaze-doctor)).

## Install

    blaze agents install                 # every agent with a ~/.claude or ~/.codex, this project
    blaze agents install codex --user    # one agent, its user files
    blaze agents install --dry-run       # the change as a diff, nothing written

With no agent named, it installs for each whose directory (`~/.claude`, `~/.codex`) exists, and
refuses naming both when neither does. The scope is the project (the workspace root of the
current directory, else the nearest `Cargo.toml`'s directory) unless `--user` says the home
directory; outside a project `--project` is refused and the default is `--user`.

| agent | hooks file | instructions file |
|---|---|---|
| Claude Code, project | `.claude/settings.json` | `CLAUDE.md` |
| Claude Code, user | `~/.claude/settings.json` | `~/.claude/CLAUDE.md` |
| Codex, project | `.codex/hooks.json` | `AGENTS.md` |
| Codex, user | `~/.codex/hooks.json` | `~/.codex/AGENTS.md` |

What is written:

- two hooks: `PostToolUse` on the edit tool (Claude Code's `Write|Edit`, Codex's `apply_patch`),
  timeout 30 s, and `PreToolUse` on `Bash`, timeout 5 s; both run `blaze agents hook <agent> v1`
  (`--strict` after it with `--strict`), the `blaze` on `PATH`, no path written;
- a block between `<!-- blaze:agents v1 begin -->` and `<!-- blaze:agents v1 end -->`: build,
  check, test and run with blaze, not cargo or rustc; what the after-edit hook says; `blaze
  diagnostics`, `blaze slow`, `blaze why` and `blaze doctor`.

Every file is merged, never replaced: other hooks, other keys (in their order) and text outside
the markers stay as they are. blaze's handler is found by its command (`blaze agents hook`) and
replaced in place, so installing again changes nothing (`already current`) and installing a newer
version updates it. A file that is not JSON, a `hooks` that is not an object, a begin marker with
no end: each refuses the whole install, naming the file and the key, before anything is written.
Each write is to a temporary file in the same directory, synced and renamed over the old one.

Codex runs a new or changed hook only after it is trusted in `/hooks`, and a project's hooks
only when its `.codex/` layer is trusted; install says so. Install warns when no `blaze` is on
`PATH`.

`blaze agents remove` acts on every agent unless one is named, removes blaze's handlers and
block, and deletes a file that only they made.

`blaze agents status` names, for each agent in the user scope and the project's, the files and
one verdict: `current`, `old` (an older `vN` or block, with `blaze agents install` as the fix),
`partial` (some hooks or the block missing), `missing`, or `newer` (written by a newer blaze
than this one). `--json` prints `{"version": 1, "agents": [{"agent", "scope", "found", "hooks",
"instructions", "after", "before", "strict", "block", "verdict", "current", "fix"}]}`.

Claude Code's plugin (`/plugin marketplace add indexable-inc/blaze`, then `/plugin install
blaze@blaze`; `plugins/.claude-plugin/plugin.json`, `plugins/claude/hooks/hooks.json`) carries
the same two hooks with no version, for whoever prefers plugins; it writes no instructions.

## After an edit

For each `.rs` file or `Cargo.toml` the edit wrote (Claude Code's `file_path` or
`notebook_path`; each `*** Add/Update/Delete File:` and `*** Move to:` line of a Codex patch), the
hook finds the package that owns it (the nearest `Cargo.toml` with a `[package]`, or a
`[workspace]` for a virtual one) and asks the
daemon, once per package, for `Request::Diagnostics` of `check -p <package> --all-targets` from
that manifest's directory, with the session's token for that manifest. All packages share one
20 s deadline.

What the agent sees:

```text
The edit was applied, and blaze check found new errors in it; fix them before going on.
blaze check -p demo: 1 new error in demo:
src/lib.rs:1:14: error[E0308]: mismatched types
  help: remove the trailing value
blaze check -p demo: 1 new, 0 fixed, 3 unchanged.
```

- new errors block (Claude Code's `decision: "block"`, Codex's too, worded "The patch was
  applied"); new warnings, fixed ones and the counts are `additionalContext`; an error said
  before and still there is counted in the summary line (`2 errors said before still there`),
  so it is not forgotten;
- the first check of a package in a session says the written files' diagnostics and counts the
  rest of the package as "elsewhere";
- errors of every package are said, warnings only of the package checked: a dependency's
  warnings are not the edit's;
- at most 10 of each kind, then `and N more: run blaze diagnostics -p <package>`; 8,000
  characters at most per kind;
- past the deadline: the check is still running in the daemon, and the next edit's hook gets its
  answer;
- a token the daemon no longer holds (it restarted, or the base was evicted): said, and
  everything shown as new;
- blaze failing (the daemon won't start, refuses the request, fails in the check): its last 15
  lines, said as blaze failing, not the code, to the agent and as `systemMessage` to the person;
- an edit that wrote no `.rs` file or `Cargo.toml`: no output. Every check says at least its
  summary line.

The hook always exits 0 and prints its answer as JSON: Codex ignores plain stdout, and a hook's
own failure is said in the answer, never as an exit code the agent reads as the edit failing.

Per session the hook keeps one file per manifest, holding the token of its last check, in
`<scratchpad>/blaze-agents/<session>/` (Claude Code's `scratchpad_dir` when the input has it,
else the system's temporary directory); `<session>` is 24 hex digits of the hash of the
session id. A token file that does not read is said, and that check shown as a first one.

## Before a shell command

The `PreToolUse` hook reads `tool_input.command`. When it runs `cargo build|check|test|run|clippy|
bench|doc` (or their one-letter forms), `rustc` or `rustdoc`, past environment assignments and
wrappers (`env`, `time`, `command`, `exec`, `nice`, `nohup`, `sudo`, `xargs`), the agent is told to use the blaze command instead, as
`additionalContext`. With `--strict` the call is refused (`permissionDecision: "deny"`, with the
same reason). Any other command, `cargo blaze` included, gets no answer.

## `blaze diagnostics`, for any agent

    blaze diagnostics [--since TOKEN] [-p PKG]... [--workspace] [--manifest-path PATH]
                      [--file FILE] [--client NAME] [--wait SECONDS] [--json]

Checks what cargo's flags name (it takes `-p`, `--workspace`, `--manifest-path`, the feature,
profile and target flags and the target selection flags, as `blaze check` does) and says what is
new since the check `TOKEN` named, what was fixed, how many are unchanged, and the token to pass
next. An agent without hooks runs it after its edits with `--since` the last token and `--file`
the file it wrote. `--client` names who asks (default `cli`; the hook sends `<agent>:<session>`):
each client of a checkout and scope has its own base. `--wait` bounds the wait (20 s); a check
still running then is said, with the same token to pass again.

The exit code is 0 when the check ended and says no error now, else 1.

`--json` prints one object:

```json
{"token": "<64 hex | null>", "base": "first|held|unknown", "root": "<64 hex>", "build": 12,
 "ended": true, "failed": null,
 "new": [{"id": "<64 hex>", "level": "error", "code": "E0308", "message": "mismatched types",
          "file": "src/lib.rs", "line": 1, "column": 14, "help": ["help: ..."],
          "package": "demo ...", "primary": true, "rendered": "..."}],
 "fixed": [], "unchanged": []}
```

`failed` is `{"what", "at"}` when blaze failed in the check (not the code). Files are relative to
the checkout. `new`, `fixed` and `unchanged` hold errors, and warnings of the packages asked for.

### The request

`blaze_api::Request::Diagnostics { view, since, client, targets, wait_ms }`, answered by
`Response::Diagnostics(Seen)` and `Done` (API schema 56). `Seen` is `{new, base, root, build,
ended, fixed, token, failed, unchanged}`; `base` is `First` (no token), `Held` or `Unknown`. The
daemon keeps each client's base in its session (`blaze_api::diagnostics::Bases`): at most 64
bases and 65,536 diagnostics together, the least recently answered dropped first; nothing is
written to disk, so a restart makes every token unknown. A check that did not end or failed keeps
the base.

### Which diagnostic is which

Each diagnostic has an id (`Found::id`) equal across an edit that only moves it. Today it is the
hash of its level, its code, its file relative to the checkout, its message with line
references made `#` (`:12:5`, `line 12`, `lines 3-5`), the trimmed text of its primary span's
first line, and that span's label made the same way. No line number: one inserted line would move
every diagnostic below it. Two diagnostics with one id are told apart by count.

The end state is the item's form: the rule's code, the path of the item the span is in and the
span relative to that item, so an edit of the line itself, or of another item, keeps it. A
diagnostic does not carry its item yet; when the front end says it, the id changes form and the
module (`crates/blaze-api/src/diagnostics.rs`) says so.

The formats each agent's hooks read and write, with the documentation they rest on, are in
`plugins/claude/FORMAT-NOTES.md` and `plugins/codex/FORMAT-NOTES.md`. The hook's decisions are
sans-IO (`crates/blaze-cargo/src/cli/agents/decide.rs`) and tested on fixtures of both agents'
inputs.
