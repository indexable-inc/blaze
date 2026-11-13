---
description: Run a full blaze check of the whole workspace and list every error and warning, not only the new ones.
allowed-tools: Bash(blaze check:*)
---

Run `blaze check --workspace --all-targets` in the workspace root and wait for it to end.

List every error first, then every warning, each as `path:line:col: level[code]: message` with
blaze's help line under it, grouped by package. End with the counts and blaze's closing
`blaze: ok` or `blaze: failed` line, and the `blaze check --fix` hint if blaze printed one. If blaze
itself failed (no closing line, a panic, a usage error), show its last lines and say so plainly.
