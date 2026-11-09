---
name: agents
description: Starting and running other agents from ts (spawn / agents.spawn): which kind, model and effort to pick for a task (Opus 5.5 at medium by default, high only where a wrong answer is silent, Fable 5.2 to plan and design hard work while others read the code and Opus implements, Codex or a small model for gathering and read-only work), the name, model and effort every sub agent must state, how to write its prompt, and how to follow, answer and end it. Read before starting any agent.
---
# Starting agents

An agent another agent starts must say its `name`, `model` and, when that model lists efforts, its `effort`; the
harness refuses the spawn otherwise, and refuses `--model`/`--effort` in `args`. Nothing is defaulted, so the
choice is always written down where the person can see it.

The name is what the person reads in the sidebar, so write it as a short plain title in words, with spaces and
capitals as in a sentence: `LaTeX rendering`, `Sidebar titles and icons`, `Fix comptime dyn`. Never kebab case, never
the repository or app name as a prefix (`harness-latex` says less than `LaTeX rendering`), never a branch or ticket
id. Two agents at once get names a person can tell apart.

```ts
const a = await spawn('claude', {
  name: 'Comptime dyn calls',      // a plain title in words: the sidebar row and how to find it again
  model: 'claude-opus-5-5',
  effort: 'medium',
  cwd: '/path/to/its/worktree',
  prompt: '...',
});
await a.reply();                    // waits for its turn to end, returns its last message
```

## Which model and effort

| The task | Kind, model | Effort |
|---|---|---|
| General work: a change, a fix, a design, a review | claude, Opus 5.5 (`claude-opus-5-5`) | medium |
| Complicated planning: a design across many crates or subsystems, a plan whose mistakes cost days | claude, Fable 5.2 (`claude-fable-5-2`), which plans and spawns the rest (below) | high |
| A wrong answer would be silent: compiler semantics, const eval, unsafe code, allocators, concurrency, persisted or crash-recovering state | claude, Opus 5.5 | high |
| Gathering: find, read, list, summarize, collect facts or logs; read-only work | codex, its small fast model from the list; or claude, Haiku 5.5 / Sonnet 5.5 | low |
| Simple and mechanical: rename across files, fix links, port a pattern where the target is spelled out | codex, or claude Sonnet 5.5 | low or medium |

Pick by what a mistake costs, not by how big the task looks: a long mechanical port is low effort, a ten-line change
to the const checker is high. Default to medium when unsure. Codex's models are the ones it lists (they are learned
when a codex first starts, `$HARNESS_HOME/models.cbor`); name one from that list, never a guess. Until codex has
listed its models the harness takes no effort for it, so give only the model.

## Fable plans, others read and build

Fable 5.2 is the strongest model and by far the most expensive per token, so it spends its tokens on decisions,
never on reading. Use it to design and plan complicated work, and Opus 5.5 (as usual) to implement the plan.

- **Fable never looks through code itself:** no reading files, searching, listing the tree or reading logs. It asks
  other agents: a gathering agent (codex's small model, Haiku or Sonnet, low effort) to find and read what it needs
  and answer in a short report of facts with paths and lines; an Opus agent to work out a question that takes
  reading plus judgment. It asks follow-up questions of them rather than opening the files.
- **It hands the building to Opus:** its output is the plan (the design, the steps, what each lane changes, what to
  prove), and each lane goes to an Opus 5.5 agent with a whole prompt (below). Fable reads their reports, not their
  diffs.
- **Its prompt says so:** when you spawn Fable, say in its prompt that it must not read code itself and must gather
  through other agents, and give it what you already know (the goal, the constraints, the reports gathered so far)
  so it starts from facts.

## The prompt

The agent sees only its prompt and its working directory. Make the prompt whole:

- **Where:** its own worktree and branch (one per lane). Never two agents in one checkout.
- **What to read first:** the skills and the two or three files that matter, by path.
- **The goal and its edges:** what done looks like, what is out of scope and who owns the neighbouring work.
- **What it may run:** in blaze, sub agents only write code and commit; the main agent runs `blaze check` and
  `blaze test` (skill `build-and-test`). Say so, or say what it may run.
- **The report:** end with what changed, what it was unsure of, and what the main agent must verify.

## Following it

- `agents.tree()` / `objects({ live: true })` for status; `a.reply()` to wait for a turn; `a.prompt(text)` to answer
  or redirect; `a.messages` for its transcript. Wait on a turn, never poll its screen in a tight loop.
- Tell the person what started (one line each: name, model, effort, goal) and what came back, not the raw transcript.
- When it is done: read its report, verify its claims yourself (build, test, read the diff), then `a.kill()` and
  clean up its worktree once its branch has landed or been dropped.
