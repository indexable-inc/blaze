---
description: Ask blaze why its last build took the time it did and what it rebuilt, and summarize the answer.
argument-hint: "[item]"
allowed-tools: Bash(blaze why:*) Bash(blaze slow:*) Bash(blaze explain:*)
---

Run `blaze why $ARGUMENTS` and `blaze slow` in the workspace, in that order. `blaze why` says what
the last build recomputed and the chain of reasons for each (with an item named, why that item
reran); `blaze slow` says where the last build's time went, from its profile.

Then answer in a few lines, quoting their rows rather than guessing: what reran and the cause blaze
gives, the spans that took the most time with their times, and the next command blaze names
(`blaze explain ...`) if one would say more. If either command fails, show its last lines and say
it was blaze that failed; do not explain the build without its answer.
