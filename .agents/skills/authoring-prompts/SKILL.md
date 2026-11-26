---
name: authoring-prompts
description: "Authoring and revising agent prompts, rules, skills, and subagent briefs — and diagnosing agent mistakes without point-patching. Covers lifting incident fixes to failure classes, the two-domain transfer test, preferring structural affordances (types, schemas, tool outputs) over prose rules, pruning before adding to respect finite attention budgets, and writing interface-first subagent briefs. Load before writing or editing AGENTS.md, scripts/src/bin/rules.rs, any .agents/skills/*/SKILL.md, a subagent or swarm brief, or proposing a rule or tooling change to prevent a repeated agent mistake."
---

# Authoring Prompts, Rules, Skills, and Briefs

Every instruction competes for finite attention. A bloated or over-specific rulebook makes agents worse, not better. Apply these four tests before writing or editing any rule, skill, prompt, brief, or workflow tweak.

## 1. Lift from incident to failure class

Never patch the literal mistake that just happened.

- If a proposed rule or tooling tweak names a specific file, command, flag, or number from today's incident, reject it and lift one level up to the **failure class**.
- Ask: *"Why did this error happen across reasoning, not just in this file?"* Name the class (e.g., *unbounded extrapolation*, *instance patching*, *stale prior over live state*, *unverified pass*) and design against the class.

## 2. The two-domain transfer test

Before adopting any instruction or structural check, test it against **two unrelated domains** (e.g., compiler latency vs. memory usage vs. API design vs. build tooling).

- If the rule or mechanism prevents real mistakes in both unrelated domains without awkward special-casing, it is general.
- If it only makes sense for the exact scenario that triggered it, it is a point patch — discard or generalize it.

## 3. Mechanism over prose, and prune before adding

Follow the strict hierarchy:

1. **Type or invariant** (makes the mistake unrepresentable).
2. **Tool output or harness schema** (surfaces the missing fact automatically at the decision point across *all* workloads of that class, not one hardcoded case).
3. **Skill** (loaded only for the specific class of task whose trigger matches; never rely on a task-scoped skill for a habit that must apply on every turn).
4. **Always-on prompt rule (`AGENTS.md`)** — last resort. Before adding a new rule, check whether tightening or merging an existing rule covers the class with zero extra lines. Every added rule dilutes attention on the rules already there.

## 4. Writing crisp subagent and swarm briefs

A brief is an interface specification, not a narrative:

- State **owned files/paths**, **non-goals / boundaries**, **exact public contracts** (types, signatures, invariants) when lanes run in parallel, and **what done looks like** (the concrete check).
- Omit background history or reasoning the worker does not need to satisfy its contract.
