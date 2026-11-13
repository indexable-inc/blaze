# Format notes: the Codex adapter

What OpenAI Codex offers after a file edit, quoted as fetched on 2026-11-13 from
[Hooks](https://learn.chatgpt.com/docs/hooks) (developers.openai.com/codex/hooks redirects there)
and [Custom instructions with AGENTS.md](https://learn.chatgpt.com/docs/agent-configuration/agents-md).

## Codex has a PostToolUse hook that feeds text back to the model

> PostToolUse runs after supported tools produce output, including Bash, apply_patch, MCP tool
> calls, and other local function tools.

> Hooks are enabled by default.

So the adapter is a hook, like Claude Code's, not a notify program and not only an `AGENTS.md`
line. The generic launcher (plugins/agent/README.md) stays for agents without hooks.

### Where it is configured

> Codex discovers hooks next to active config layers in either of these forms: hooks.json ...
> inline [hooks] tables inside config.toml

> In practice, the four most useful locations are: ~/.codex/hooks.json ~/.codex/config.toml
> <repo>/.codex/hooks.json <repo>/.codex/config.toml

> Project-local hooks load only when the project .codex/ layer is trusted.

> Before a non-managed hook can run, Codex requires you to review and trust the exact hook
> definition. ... Use /hooks in the CLI to inspect hook sources, review new or changed hooks,
> trust hooks

`install.py '{"agent": "codex", ...}'` writes `hooks.json` (user or project) and says to trust
the hook in `/hooks`.

> For repo-local hooks, prefer resolving from the git root instead of using a relative path such
> as .codex/hooks/.... Codex may be started from a subdirectory

Hence the project hook's command `python3 -I "$(git rev-parse --show-toplevel)/plugins/codex/check.py"`.

### The matcher: `apply_patch`

> For file edits through apply_patch, matcher values can use apply_patch, Edit, or Write; hook
> input still reports tool_name: "apply_patch".

### The input on stdin

> Every command hook receives one JSON object on stdin.

> session_id | string | Current Codex session id. Subagent hooks use the parent session id. ...
> cwd | string | Working directory for the session

> tool_input | JSON value | Tool-specific input. Bash and apply_patch use tool_input.command.

### What the model sees

> JSON on stdout can use systemMessage and this hook-specific shape: { "decision": "block",
> "reason": "...", "hookSpecificOutput": { "hookEventName": "PostToolUse", "additionalContext":
> "..." } }

> That additionalContext text is added as extra developer context.

> For this event, decision: "block" doesn't undo the completed Bash command. Instead, Codex
> records the feedback, replaces the tool result with that feedback, and continues the model from
> the hook-provided message.

Because the block replaces the tool result, the reason starts "The patch was applied".

> Plain text on stdout is ignored.

### Size and time

> By default, Codex limits each model-visible hook-output message to roughly 2,500 tokens.

The core's 8,000-character budget per kind stays under it.

> timeout is in seconds. If timeout is omitted, Codex uses 600 seconds for most hooks.

The handler sets 30, over the adapter's 20 s deadline, for the reasons in
plugins/claude/FORMAT-NOTES.md ("The timeout").

> Commands run with the session cwd as their working directory.

## Not confirmed from the docs (assumed)

- **The patch format.** The hooks page says `apply_patch` puts the patch in
  `tool_input.command` but not what a patch looks like. The adapter reads Codex's apply_patch
  envelope as Codex writes it (`*** Begin Patch`, `*** Add File: P`, `*** Update File: P`,
  `*** Delete File: P`, `*** Move to: P`, `*** End Patch`), with paths relative to `cwd`. A
  patch where none of these lines is found is said to the model ("checked nothing"), never passed.
- That `tool_input.command` is a string for `apply_patch` (the page says only "use
  tool_input.command"); a non-string is said the same way.
- That a block's `reason` is shown to the model when `additionalContext` is also set: the page's
  example sets both.
- **Plugins.** The hooks page says "Installed plugins can also bundle lifecycle config through
  their plugin manifest or a default hooks/hooks.json file", but the "Build plugins" page it
  links serves ChatGPT's plugin builder, with no Codex manifest format, so no Codex plugin is
  shipped; the hook is installed into `hooks.json` instead.
