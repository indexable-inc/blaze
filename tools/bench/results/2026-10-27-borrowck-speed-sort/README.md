# The borrow checker's time on the bench apps, before and after the order drop live locals are walked in is found once per local

Made by `harness/borrowck_speed.py --jobs 1` (one `blaze-engine borrowck --project DIR --timings`
per app, see docs/borrowck.md "Speed"), in an arm64 ix VM (32 vCPUs, 32 GiB), inside
`nix develop`. `before.*` is main 7cd0efbb; `after.*` is main 7cd0efbb with the key drop live
locals are sorted by, the first point a walk meets each at, found once per local
(`sort_by_cached_key`) rather than on every comparison, where it went over every point the
local is drop live at. Each `.json` is the harness's summary, each `.md` its tables with the
slowest bodies.

Helix's `keymap::default`, 37k points, goes from 109 ms to 103 ms; the rest is within noise.

Times in ms, the checker without MIR lowering as an edit of each body pays it, beside an edit's
type inference; p99 and the slowest body in us; the phases that moved most.

| App | Mode | Infer (edit) | Before | After | / infer after | p99 before -> after | max before -> after | walk | constraints | moves | liveness |
|---|---|---|---|---|---|---|---|---|---|---|---|
| ripgrep | polonius | 585 | 311 | 311 | 53% | 1017 -> 1004 | 7068 -> 7272 | 21 -> 21 | 123 -> 123 | 43 -> 43 | 61 -> 61 |
| ripgrep | nll | 585 | 267 | 267 | 46% | 894 -> 890 | 6564 -> 7026 | 19 -> 19 | 107 -> 107 | 38 -> 38 | 56 -> 56 |
| fd | polonius | 177 | 77 | 78 | 44% | 6452 -> 6433 | 11624 -> 11826 | 5 -> 5 | 32 -> 33 | 10 -> 10 | 17 -> 17 |
| fd | nll | 177 | 69 | 70 | 40% | 6368 -> 6415 | 11172 -> 11293 | 4 -> 4 | 30 -> 30 | 10 -> 10 | 16 -> 16 |
| bat | polonius | 263 | 155 | 153 | 58% | 2201 -> 2202 | 33993 -> 33985 | 9 -> 9 | 60 -> 59 | 19 -> 19 | 44 -> 44 |
| bat | nll | 263 | 138 | 137 | 52% | 2033 -> 1985 | 33481 -> 33610 | 8 -> 8 | 53 -> 53 | 18 -> 17 | 42 -> 42 |
| hyperfine | polonius | 197 | 75 | 74 | 38% | 3379 -> 3371 | 8039 -> 7997 | 4 -> 4 | 37 -> 37 | 8 -> 8 | 12 -> 12 |
| hyperfine | nll | 197 | 65 | 65 | 33% | 3085 -> 3110 | 7184 -> 7156 | 4 -> 4 | 33 -> 33 | 7 -> 7 | 11 -> 11 |
| just | polonius | 1012 | 519 | 520 | 51% | 4274 -> 4287 | 17445 -> 17686 | 34 -> 34 | 230 -> 233 | 62 -> 62 | 112 -> 110 |
| just | nll | 1012 | 467 | 466 | 46% | 4074 -> 4098 | 16616 -> 16749 | 32 -> 32 | 212 -> 214 | 58 -> 57 | 106 -> 105 |
| axum | polonius | 3772 | 1024 | 1017 | 27% | 9626 -> 9433 | 14838 -> 14743 | 45 -> 45 | 482 -> 483 | 107 -> 107 | 292 -> 284 |
| axum | nll | 3772 | 954 | 946 | 25% | 9094 -> 9050 | 14263 -> 14187 | 43 -> 43 | 462 -> 463 | 100 -> 100 | 282 -> 273 |
| helix | polonius | 2983 | 1406 | 1391 | 47% | 1947 -> 1903 | 109034 -> 103433 | 89 -> 89 | 596 -> 592 | 182 -> 180 | 305 -> 298 |
| helix | nll | 2983 | 1232 | 1220 | 41% | 1728 -> 1715 | 105594 -> 100178 | 83 -> 83 | 528 -> 526 | 166 -> 165 | 288 -> 280 |
| nushell | polonius | 13839 | 6457 | 6330 | 46% | 1765 -> 1726 | 96077 -> 92362 | 380 -> 377 | 2896 -> 2824 | 816 -> 804 | 1467 -> 1440 |
| nushell | nll | 13839 | 5683 | 5604 | 40% | 1602 -> 1577 | 92104 -> 90390 | 353 -> 350 | 2557 -> 2511 | 743 -> 736 | 1368 -> 1348 |

A body's cold first check, in ms: ripgrep 833 -> 841, fd 1186 -> 1188, bat 462 -> 452, hyperfine 221 -> 220, just 3367 -> 3356, axum 8117 -> 8160, helix 3722 -> 3706, nushell 23481 -> 22716.
