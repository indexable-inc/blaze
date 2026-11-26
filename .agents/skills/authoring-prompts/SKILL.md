---
name: authoring-prompts
description: "Authoring and revising agent prompts, rules, skills, and subagent briefs — and diagnosing agent mistakes without point-patching. Covers lifting incident fixes to failure classes, the two-domain transfer test, preferring structural affordances (types, schemas, tool outputs) over prose rules, pruning before adding to respect finite attention budgets, and writing interface-first subagent briefs. Load before writing or editing AGENTS.md, scripts/src/bin/rules.rs, any .agents/skills/*/SKILL.md, a subagent or swarm brief, or proposing a rule or tooling change to prevent a repeated agent mistake."
---

# Authoring Prompts, Rules, Skills, and Briefs

Every instruction competes for finite attention. A bloated or over-specific rulebook makes agents worse, not better. Apply these four tests before writing or editing any rule, skill, prompt, brief, or workflow tweak.

## 1. Generalize with >=3 similar cases, name the class, fix from first principles

Never patch the literal mistake that just happened.

- Whenever you hit a problem, find **at least 3 similar cases** across the codebase or system that share the same underlying cause.
- Extract what generalizes across all of them, **name the problem class** (e.g., *unbounded extrapolation*, *instance patching*, *stale prior over live state*, *unverified pass*), fix that entire category from first principles, and merge the fix across all siblings.
- If a proposed rule or tooling tweak names a specific file, command, flag, or number from today's incident and cannot cover >=3 cases, it is a point patch — reject or generalize it.

## 2. Mechanism over prose, and prune before adding

Follow the strict hierarchy:

1. **Type or invariant** (makes the mistake unrepresentable).
2. **Tool output or harness schema** (surfaces the missing fact automatically at the decision point across *all* workloads of that class, not one hardcoded case).
3. **Skill** (loaded only for the specific class of task whose trigger matches; never rely on a task-scoped skill for a habit that must apply on every turn).
4. **Always-on prompt rule (`AGENTS.md`)** — last resort. Before adding a new rule, check whether tightening or merging an existing rule covers the class with zero extra lines. Every added rule dilutes attention on the rules already there.

## 3. Name by exact action or role, sweep siblings, and write interface-first briefs

- Name every skill, rule, and doc by its unambiguous action or role (e.g., `authoring-prompts`, `changing-code`, `debugging`) — never a vague topic noun that leaves direction or purpose ambiguous.
- **Retroactive sweep without asking:** Whenever you introduce or tighten a rule, naming convention, or design invariant, immediately audit its existing siblings (rules, skills, docs, scripts, callers) against the new invariant and fix every clear violation in the same batch — never patch only the single artifact that triggered it.
- A subagent or swarm brief is an interface specification, not a narrative:
  - State **owned files/paths**, **non-goals / boundaries**, **exact public contracts** (types, signatures, invariants) when lanes run in parallel, and **what done looks like** (the concrete check).
  - Omit background history or reasoning the worker does not need to satisfy its contract.
