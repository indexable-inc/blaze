---
status: plan
summary: Every speed-of-light bound blaze is held to, one machine-read block each; harness/v2/bounds.py reads this file and budgets.py measures it.
date: 2026-11-09
---
# Bounds

A bound is the least time (or the most throughput) a component's work can take on the hardware,
from arithmetic: bytes over memory bandwidth, probes times a cache miss, items over cores. The
budget is the bound × 1.1 (a rate: ÷ 1.1). More than 10% off is a bug
([plan.md](../plan.md#rules)).

A bound counts only when a program measures it. Each is one `bound` block here, with the
arithmetic in the sentence above it; `harness/v2/bounds.py` reads the blocks and
`harness/v2/budgets.py` (budget `bounds`) runs each program until the answer is settled. Prose
bounds nobody can run are not kept: to hold a component to its bound, add its block and the
program that prints the number (the block's fields are in `harness/v2/bounds.py`).

## proof

First difference of two 512 MiB files on 16 cores: 1.07 GB read at 400 GB/s is 2.7 ms, plus
0.25 ms of page faults, 2.9 ms.

```bound
{"name": "proof: first difference, 512 MiB pair", "bound": 2.9, "unit": "ms", "program": "blaze-v2-proof", "args": ["{\"bench\": \"first-difference\"}"]}
```

Joining two build traces of 5 M keys and 50 M edges, read in place and split by key range across
16 cores: 3.75 ms of reading, 25 ms of probes (50 M × 8 ns ÷ 16), 1 ms of checks, 30 ms.

```bound
{"name": "proof: trace join, 5 M keys and 50 M edges", "bound": 30.0, "unit": "ms", "program": "blaze-v2-proof", "args": ["{\"bench\": \"join\"}"]}
```
