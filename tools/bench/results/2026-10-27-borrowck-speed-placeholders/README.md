# The borrow checker's time on the bench apps, before and after a goal whose answer names a placeholder is proven by the query

Made by `harness/borrowck_speed.py --jobs 1` (one `blaze-engine borrowck --project DIR --timings`
per app, see docs/borrowck.md "Speed"), in an arm64 ix VM (32 vCPUs, 32 GiB), inside
`nix develop`. `before.*` is main 0861a8f4; `after.*` is main 0861a8f4 with `prove_query`
answering a goal whose answer names a placeholder of a `for<'a>`, or a variable of a universe
past the root, by the query's own universes, which a body makes anew in the same order, as
rustc's canonical query response. Before, such a goal was proven again in the body on every
check. Each `.json` is the harness's summary, each `.md` its tables with the slowest bodies.

Only nushell's `tool_router` moves: 106.9 ms -> 0.34 ms (Polonius), 106.7 ms -> 0.18 ms (NLL),
one goal, `(Tool, fn): IntoToolRoute<NushellMcpServer, AsyncMethodAdapter<..>>`, of rmcp's
`#[tool_router]`. The rest is noise.

Times in ms, the checker without MIR lowering as an edit of each body pays it, beside an edit's
type inference; p99 and the slowest body in us; the phases that moved most.

| App | Mode | Infer (edit) | Before | After | / infer after | p99 before -> after | max before -> after | walk | constraints | moves | liveness |
|---|---|---|---|---|---|---|---|---|---|---|---|
| ripgrep | polonius | 580 | 319 | 317 | 55% | 1040 -> 1032 | 7171 -> 7084 | 28 -> 28 | 122 -> 121 | 43 -> 43 | 64 -> 64 |
| ripgrep | nll | 580 | 277 | 275 | 47% | 940 -> 942 | 6845 -> 7183 | 26 -> 26 | 107 -> 106 | 39 -> 39 | 59 -> 58 |
| fd | polonius | 183 | 85 | 88 | 48% | 6965 -> 7205 | 14492 -> 15589 | 12 -> 12 | 32 -> 34 | 10 -> 11 | 17 -> 18 |
| fd | nll | 183 | 78 | 79 | 43% | 6948 -> 7130 | 14018 -> 13952 | 12 -> 12 | 30 -> 30 | 10 -> 10 | 17 -> 17 |
| bat | polonius | 264 | 195 | 194 | 73% | 2410 -> 2451 | 63279 -> 62615 | 46 -> 46 | 60 -> 59 | 19 -> 19 | 47 -> 47 |
| bat | nll | 264 | 179 | 179 | 68% | 2182 -> 2215 | 63804 -> 63282 | 45 -> 45 | 54 -> 54 | 18 -> 18 | 45 -> 45 |
| hyperfine | polonius | 197 | 78 | 79 | 40% | 3685 -> 3698 | 8195 -> 8273 | 8 -> 8 | 37 -> 37 | 8 -> 8 | 13 -> 13 |
| hyperfine | nll | 197 | 69 | 69 | 35% | 3511 -> 3458 | 7333 -> 7413 | 7 -> 7 | 33 -> 33 | 8 -> 8 | 12 -> 12 |
| just | polonius | 985 | 578 | 572 | 58% | 5406 -> 5330 | 17720 -> 17675 | 68 -> 67 | 228 -> 225 | 62 -> 61 | 140 -> 138 |
| just | nll | 985 | 528 | 524 | 53% | 5262 -> 5189 | 17059 -> 17039 | 66 -> 65 | 211 -> 209 | 58 -> 58 | 134 -> 133 |
| axum | polonius | 3766 | 1684 | 1673 | 44% | 19118 -> 18705 | 32330 -> 31722 | 702 -> 695 | 485 -> 479 | 107 -> 108 | 289 -> 290 |
| axum | nll | 3766 | 1622 | 1614 | 43% | 18737 -> 18026 | 31743 -> 31281 | 697 -> 691 | 468 -> 464 | 104 -> 105 | 283 -> 284 |
| helix | polonius | 2927 | 1478 | 1439 | 49% | 2093 -> 2020 | 109630 -> 106622 | 149 -> 145 | 592 -> 573 | 183 -> 179 | 317 -> 310 |
| helix | nll | 2927 | 1311 | 1283 | 44% | 1908 -> 1858 | 106059 -> 102004 | 142 -> 139 | 532 -> 520 | 168 -> 165 | 300 -> 294 |
| nushell | polonius | 14172 | 9408 | 9381 | 66% | 2322 -> 2346 | 207680 -> 214658 | 2508 -> 2509 | 3013 -> 2955 | 829 -> 832 | 2144 -> 2170 |
| nushell | nll | 14172 | 8646 | 8604 | 61% | 2191 -> 2210 | 205895 -> 213557 | 2422 -> 2423 | 2749 -> 2672 | 764 -> 767 | 2023 -> 2050 |

A body's cold first check, in ms: ripgrep 838 -> 832, fd 1222 -> 1255, bat 514 -> 500, hyperfine 222 -> 221, just 3386 -> 3370, axum 8246 -> 8127, helix 3740 -> 3600, nushell 25766 -> 26382.
