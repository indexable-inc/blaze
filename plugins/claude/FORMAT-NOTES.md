# Format notes: the Claude Code plugin

The doc lines each format decision of this plugin rests on, quoted as fetched on 2026-11-13, so a
reader can check the plugin against them. Pages: [plugins](https://code.claude.com/docs/en/plugins),
[plugin manifest reference](https://code.claude.com/docs/en/plugins-reference),
[components](https://code.claude.com/docs/en/plugins/components),
[loading](https://code.claude.com/docs/en/plugins/loading),
[create a marketplace](https://code.claude.com/docs/en/plugin-marketplaces),
[marketplace reference](https://code.claude.com/docs/en/plugins/marketplace-reference),
[install](https://code.claude.com/docs/en/plugins/install),
[hooks](https://code.claude.com/docs/en/hooks),
[tools reference](https://code.claude.com/docs/en/tools-reference),
[skills](https://code.claude.com/docs/en/skills).

## The plugin root is `plugins/`, not `plugins/claude/`

The hooks run `blaze agents hook claude` from `PATH` (crates/blaze-cargo/src/cli/agents.rs), so
the plugin carries no script of its own; the root stays `plugins/` so the marketplace entry and
the manifest keep their paths:

> Every other marketplace plugin: Claude Code copies the plugin into
> `cache/<marketplace>/<plugin>/<version>/` at install and loads that copy. Files outside the
> plugin directory aren't copied, so when a script inside a copied plugin reads a path above the
> plugin root, such as `../shared`, it doesn't find them (loading)

So the manifest is `plugins/.claude-plugin/plugin.json`, and it points at the hook file and the
commands under `plugins/claude/`.

## Manifest: `plugins/.claude-plugin/plugin.json`

> Save the manifest at `.claude-plugin/plugin.json` under the plugin root. Put every other plugin
> file at the plugin root, not inside `.claude-plugin/`. That includes `skills/`, `commands/`,
> and `hooks/`. (plugins-reference)

> The table lists top-level keys in `plugin.json`. `name` is the only required key.
> (plugins-reference)

> `hooks` | Path, object, or array of either | `.json` hook files or inline hook config. Loaded
> together with `hooks/hooks.json` (plugins-reference)

> `commands` | Path, array of paths, or object | Flat `.md` command files, directories of them,
> or an object map of command name to `source` or `content`. Replaces the default `commands/`
> scan (plugins-reference)

> Every component path in a manifest is relative to the plugin root and must start with `./`.
> (plugins-reference)

The name `blaze` is not reserved: the reserved ones start with `claude-`, `anthropic-`, ... or
"Has `claude`, `anthropic`, or `anthropics` as a whole word" (plugins-reference, `name`).

## Hooks: `plugins/claude/hooks/hooks.json`

> A hooks file wraps the event map in a top-level `"hooks"` key, the shape `hooks/hooks.json`
> uses. A file that contains only the event map, without that wrapper, fails to load.
> (plugins-reference)

The handler is the shell form `blaze agents hook claude`, with no path in it: the same string
`blaze agents install` writes into a settings file (with `vN` after it, so `blaze agents status`
can say an install is out of date), and the same string works in Codex's `hooks.json`, which has
no exec form. The plugin's handler carries no `vN`: it is read as following the blaze on `PATH`.

### The matcher: `Write|Edit`

> Only letters, digits, `_`, `-`, spaces, `,`, and `\|` | Exact string, or list of exact strings
> separated by `\|` or `,` ... `Edit\|Write` and `Edit, Write` each match either tool exactly
> (hooks)

> `PostToolUse` ... Matches on tool name, same values as PreToolUse. (hooks)

> built-in tools such as `Bash`, `PowerShell`, `Edit`, `Write`, `Read`, ... (hooks, PreToolUse)

`MultiEdit` is not named on any of these pages today, so it is not in the matcher. `NotebookEdit`
is a tool ("Modifies Jupyter notebook cells", tools-reference) and only writes notebooks, never a
`.rs` file or a `Cargo.toml`, so matching it would start the hook for nothing on every notebook
edit.

### Before a shell command: `PreToolUse` on `Bash`

Read from the hooks page in the same session as the quotes above, and summarized here, not
quoted word for word (the fetch returned a summary): `PreToolUse` runs before a tool runs and
matches on the tool name, `Bash` among them; a `Bash` call's input has the command line in
`tool_input.command`; the hook's JSON answer can carry `hookSpecificOutput.additionalContext`
(text added for Claude) or `hookSpecificOutput.permissionDecision: "deny"` with
`permissionDecisionReason` (the call is refused and the reason shown to Claude), with
`hookEventName: "PreToolUse"`. `blaze agents hook claude` answers a `cargo build|check|test|run|
clippy|bench|doc` or `rustc` command with the context by default and with the denial under
`--strict`; it answers nothing for any other command. Its timeout is 5 s: it reads no file and
asks no daemon.

### The input on stdin

> For command hooks, this JSON arrives via stdin. (hooks)

> `session_id` | Current session identifier ... `cwd` | Current working directory when the hook
> is invoked ... `scratchpad_dir` | Path to the session's scratchpad directory, where Claude
> keeps temporary working files. Absent when the session has no scratchpad or the temp directory
> is unavailable. Requires Claude Code v2.1.257 or later (hooks)

The PostToolUse example input: `"tool_name": "Write"`, `"tool_input": {"file_path":
"/path/to/file.txt", "content": "file content"}` (hooks), and:

> For the file tools `Write`, `Edit`, and `Read`, `tool_input.file_path` is always absolute
> (hooks)

The session's state (the token of each package's last check; the base it names is the daemon's)
goes under `scratchpad_dir` when it is there, else the system's temporary directory;
`${CLAUDE_PLUGIN_DATA}` is for what outlives sessions ("a directory that survives updates, for
`node_modules`, virtual environments, and caches", components), which this state does not.

### What Claude sees

New errors block, with the diagnostics as the reason:

> `decision` | `"block"` adds the `reason` next to the tool result. Claude still sees the
> original output ... `reason` | Explanation shown to Claude when `decision` is `"block"` (hooks,
> PostToolUse decision control)

Warnings and counts are additional context:

> `additionalContext` | String added to Claude's context alongside the tool result (hooks)

> Claude Code wraps the string in a system reminder and inserts it into the conversation at the
> point where the hook fired. (hooks, Add context for Claude)

The JSON form, not exit code 2, because a hook should pick one, and JSON carries both kinds:

> Choose one approach per hook: either use exit codes alone for signaling, or exit 0 and print
> JSON for structured control. (hooks)

> `PostToolUse` | No | Shows stderr to Claude; the tool already ran (hooks, exit code 2 per event)

> `systemMessage` | none | Warning message shown to the user. (hooks)

The cap on what the hook prints:

> A hook's `additionalContext`, `systemMessage`, and `initialUserMessage` strings, and its plain
> stdout, are capped at 10,000 characters ... Claude Code saves the output to a file in the
> session directory. It replaces the output with the file path and a preview of up to the first
> 2,000 characters. (hooks)

Hence at most 10 diagnostics of each level and 8,000 characters of each kind
(`CAP`, `TEXT_BUDGET` in crates/blaze-cargo/src/cli/agents/decide.rs).

### The timeout: 30 s, the core's deadline 20 s

> `timeout` | no | Seconds before canceling. ... Defaults: 600 for `command` (hooks)

> Claude Code cancels a `command`, `http`, or `mcp_tool` hook that reaches its `timeout`,
> discarding the hook's output (hooks, Timeouts)

The hook runs synchronously after each write, so its timeout is how long one write can stall the
agent. A warm daemon answers in tens of milliseconds (13 ms unchanged, 42 to 92 ms a body edit on
small projects, measured by the coordinator); only a first check of a cold project takes seconds
to minutes. 20 s bounds that stall and is still long enough for most first checks; at 20 s the
hook says "still running in the daemon" itself (the daemon answers the request at its `wait_ms`)
and leaves the check running, and the 10 s margin to the 30 s timeout covers starting blaze and
the daemon, so Claude Code never cancels the hook and discards what it would have said.

## Commands: `plugins/claude/commands/*.md`

> Save a command at `commands/<file>.md` and it becomes `/<plugin>:<file>`. ... Command files
> take the same frontmatter as skills. (components)

> Commands are the older format, and skills supersede them for new work. (components)

Kept as commands, as asked; `/blaze:blaze-why` and `/blaze:blaze-check`. Frontmatter fields used:
`description`, `argument-hint`, `allowed-tools` and `$ARGUMENTS` in the body (skills).

## The marketplace: `.claude-plugin/marketplace.json` at the repository root

> A plugin marketplace is a directory or repository with a `.claude-plugin/marketplace.json`
> file that lists your plugins and where to fetch each one. (plugin-marketplaces)

> The table lists every key Claude Code reads from `marketplace.json`. `name`, `owner`, and
> `plugins` are required. (marketplace-reference)

> Each object in `plugins` is a plugin entry and needs a `name` and a `source`. Write the entry's
> `source` as a path from the marketplace root. (plugin-marketplaces)

> Keep the entry name and the manifest name the same (plugin-marketplaces)

Install:

> GitHub repository | `owner/repo`. ... | `/plugin marketplace add anthropics/claude-plugins-official`
> (install)

> The install id is the entry's `name`, an `@`, and the marketplace `name`. (plugin-marketplaces)

So `/plugin marketplace add indexable-inc/blaze`, then `/plugin install blaze@blaze`.

## Turning it off

> run `/plugin enable`, `/plugin disable`, or `/plugin uninstall` to open the panel and make that
> change there (install); from a shell, `claude plugin disable formatter@your-org` (install)

> To temporarily disable all hooks without removing them, set `"disableAllHooks": true` in your
> settings file. ... There is no way to disable an individual hook while keeping it in the
> configuration. (hooks)

## Not confirmed from the docs (assumed)

- That `"hooks": "./claude/hooks/hooks.json"` and `"commands": ["./claude/commands/"]` load from
  subdirectories of the root as they would from `hooks/` and `commands/`: the reference allows
  any `./` path to a `.json` file and directories of commands; `claude plugin validate plugins`
  is the check, not run here (no `claude` was run).
- That `blaze` is free as a marketplace name for users who add it: names collide per user
  ("can't have two marketplaces with the same name").
- The repository slug `indexable-inc/blaze` is the one the task gave; this checkout's remote is
  a fork.
