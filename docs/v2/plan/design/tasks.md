---
status: plan
summary: Tasks in the agent app. Every agent belongs to a task, the sidebar lists tasks with their agents under them, and a task says what it waits on, what it costs and how it ended.
date: 2026-11-09
---
# Tasks in the agent app

The app in `crates/harness/` lists agents: a tree of sessions named after processes. A person
running twenty agents wants to know what is being done, what is stuck and what needs them. None of
that is on screen today: there is no task, no "waiting on you" state, no step, no cost and no
branch. This is the design that puts them there. Objectives A1 to A5 of
[objectives.md](../objectives.md).

## The model

A **task** is one piece of work a person would name: "T5: no-op build under 5 ms", "review PR 812".
It is an object in the daemon beside sessions, kept in the same SQLite table (`objects`, kind
`task`), so it survives restarts and every client sees the same list.

| field | meaning |
|---|---|
| `id`, `title` | the title is what the sidebar shows; an objective id leads it when there is one |
| `outcome`, `check` | one line each: what done is, and what decides it |
| `state` | `running`, `needs you`, `blocked`, `done`, `failed`, `dropped` |
| `steps` | a short ordered list, each `todo`, `doing` or `done`; the agent keeps it current |
| `branch`, `pr` | where the work is, set when the agent makes them |
| `cost_usd`, `started`, `ended` | summed over the task's agents |
| `result` | one line when it ends: what the check printed, or what blocks it |

Every session has exactly one task (`Request::Spawn` gains `task`):

- a spawn that names a task joins it;
- a spawn from an agent with no `task` joins the parent's;
- a session a person opens with no task gets a new one, titled from its first message.

So no agent is ever without a task, and nothing an agent already does breaks.

`state` is derived where it can be, set where it cannot. `needs you` is derived: some agent of the
task is waiting on a permission prompt, a question, or has ended its turn with the task not done.
`running` is derived: some agent is busy. `done`, `failed`, `blocked` and `dropped` are set, by
the agent (`task.finish({state, result})`) or by a person.

## What agents call

Three calls on the existing socket API, beside spawn:

- `task.create({title, outcome, check})`: returns an id; used by a coordinator before it starts
  workers.
- `task.update({steps?, branch?, pr?})`: the agent's plan and where its work is. This is the
  agent's todo list, and it is the person's progress bar.
- `task.finish({state, result})`.

## The sidebar

Tasks are the top-level rows; agents are indented under their task and collapsed by default when
the task has one agent. Order: `needs you` first, then `running`, then `blocked`, then ended, each
newest first. A task row shows, in one line: a state mark, the title, the current step
(`3/7 port callers`), the time since it started, and the cost. A second, dim line shows the branch
and the pull request when there are any. Selecting a task with one agent opens that agent's chat;
with several, the agent list.

A task that enters `needs you` raises one system notification with its title and what it waits
for. An ended task stays in the list with its result until a person archives it; nothing is
archived on its own.

Nothing moves: rows appear and change in place, with no animation.

## Where it goes in the code

| piece | place |
|---|---|
| the task object, its table rows and the three calls | `harness/src/daemon.rs`, `harness/src/object.rs`, `harness-proto/src/msg.rs` |
| `task` on spawn, inherited from the parent | `Daemon::spawn_full`, `mcp/src/obj.ts` |
| `Task` entities, `(InTask, task)` on each session, the ordering | `harness-native-model/src/lib.rs` (`rows()`) |
| the row template, in its own script (Flecs allows 64 reactive inputs per script) | `harness-native/scene/widgets/task_row.flecs` |
| sync from the daemon's list | `harness-native/src/app.rs` (`sync_view`) |
| the notification | the macOS driver, on the model's `needs you` edge |

The model is a sans-IO core: it takes the daemon's task and session lists and returns rows, so
the ordering, the derived states and the grouping are unit tests with no window. The sidebar's
look is checked by an offscreen shot.

## Order of work

1. The task object, `task` on spawn with inheritance, and the three calls (A1).
2. The model's grouping and ordering, and the task row (A2).
3. `needs you` and the notification (A3).
4. Steps, time, cost, branch and pull request on the row (A4); results and archiving (A5).
