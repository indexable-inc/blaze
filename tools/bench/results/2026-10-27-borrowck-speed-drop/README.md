# The borrow checker's time on the bench apps, before and after drop liveness is found as rustc's dropck finds it

Made by `harness/borrowck_speed.py --jobs 1` (one `blaze-engine borrowck --project DIR --timings`
per app, see docs/borrowck.md "Speed"), in an arm64 ix VM (32 vCPUs, 32 GiB), inside
`nix develop`. `before.*` is main d93d0958; `after.*` is main d93d0958 with what dropping a
type uses found over the strongly connected components of the types it drops, an ADT's part a
query in its own parameters (rustc's `adt_dtorck_constraint`), whether dropping a type runs
code a query per type modulo regions, and drop liveness asking whether a local may be
initialized leaving a block only of the locals drop live there. Each `.json` is the harness's
summary, each `.md` its tables with the slowest bodies.

Times in ms, the checker without MIR lowering as an edit of each body pays it, beside an edit's
type inference; p99 and the slowest body in us; the phases that moved most.

| App | Mode | Infer (edit) | Before | After | / infer after | p99 before -> after | max before -> after | walk | constraints | moves | liveness |
|---|---|---|---|---|---|---|---|---|---|---|---|
| ripgrep | polonius | 583 | 318 | 308 | 53% | 1052 -> 1006 | 7416 -> 7405 | 28 -> 21 | 122 -> 121 | 43 -> 43 | 63 -> 61 |
| ripgrep | nll | 583 | 275 | 265 | 45% | 932 -> 880 | 6745 -> 6949 | 26 -> 19 | 106 -> 106 | 38 -> 38 | 58 -> 56 |
| fd | polonius | 178 | 85 | 79 | 44% | 6918 -> 6608 | 14265 -> 12300 | 12 -> 5 | 33 -> 33 | 10 -> 10 | 17 -> 17 |
| fd | nll | 178 | 78 | 71 | 40% | 6933 -> 6533 | 13863 -> 12040 | 12 -> 4 | 30 -> 30 | 10 -> 10 | 16 -> 16 |
| bat | polonius | 269 | 195 | 157 | 58% | 2414 -> 2321 | 61452 -> 34287 | 45 -> 9 | 60 -> 61 | 19 -> 19 | 47 -> 45 |
| bat | nll | 269 | 178 | 138 | 51% | 2260 -> 2068 | 60936 -> 34041 | 44 -> 8 | 54 -> 54 | 17 -> 17 | 45 -> 42 |
| hyperfine | polonius | 208 | 79 | 77 | 37% | 3803 -> 3672 | 8176 -> 8568 | 8 -> 4 | 37 -> 39 | 8 -> 9 | 13 -> 12 |
| hyperfine | nll | 208 | 70 | 66 | 32% | 3600 -> 3276 | 7440 -> 7567 | 7 -> 4 | 34 -> 34 | 8 -> 8 | 12 -> 11 |
| just | polonius | 1064 | 602 | 541 | 51% | 5517 -> 4446 | 18529 -> 19111 | 70 -> 34 | 243 -> 246 | 63 -> 63 | 143 -> 115 |
| just | nll | 1064 | 545 | 482 | 45% | 5309 -> 4182 | 17419 -> 18312 | 68 -> 33 | 221 -> 222 | 59 -> 59 | 137 -> 109 |
| axum | polonius | 3726 | 1681 | 1012 | 27% | 18805 -> 9543 | 31497 -> 14790 | 699 -> 44 | 482 -> 474 | 108 -> 105 | 290 -> 291 |
| axum | nll | 3726 | 1621 | 945 | 25% | 18302 -> 9119 | 31239 -> 14257 | 694 -> 42 | 466 -> 454 | 105 -> 100 | 284 -> 282 |
| helix | polonius | 3006 | 1453 | 1399 | 47% | 2006 -> 1904 | 105820 -> 108117 | 147 -> 89 | 582 -> 591 | 179 -> 180 | 311 -> 306 |
| helix | nll | 3006 | 1291 | 1227 | 41% | 1856 -> 1726 | 103708 -> 105580 | 140 -> 83 | 522 -> 523 | 166 -> 165 | 295 -> 290 |
| nushell | polonius | 14084 | 9690 | 6502 | 46% | 2382 -> 1761 | 213364 -> 107597 | 2576 -> 375 | 3146 -> 2955 | 843 -> 808 | 2196 -> 1471 |
| nushell | nll | 14084 | 8820 | 5732 | 41% | 2249 -> 1581 | 211531 -> 107816 | 2467 -> 349 | 2823 -> 2615 | 772 -> 736 | 2061 -> 1371 |

A body's cold first check, in ms: ripgrep 840 -> 827, fd 1193 -> 1178, bat 488 -> 475, hyperfine 226 -> 243, just 3579 -> 3541, axum 8160 -> 8108, helix 3667 -> 3718, nushell 27326 -> 23596.
