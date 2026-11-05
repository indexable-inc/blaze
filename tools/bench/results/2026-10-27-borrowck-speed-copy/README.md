# The borrow checker's time on the bench apps, before and after `is_copy` and normalizing are queries and dominators are semi-NCA

Made by `harness/borrowck_speed.py --jobs 1` (one `blaze-engine borrowck --project DIR --timings`
per app, see docs/borrowck.md "Speed"), in an arm64 ix VM (32 vCPUs, 32 GiB), inside
`nix develop`. `before.*` is main 5b58cc84; `after.*` is main 5b58cc84 with whether a type is
`Copy` and normalizing canonical salsa queries, and dominators by semi-NCA. Each `.json` is
the harness's summary, each `.md` its tables with the slowest bodies.

Times in ms, the checker without MIR lowering as an edit of each body pays it, beside an edit's
type inference; p99 and the slowest body in us; the phases that moved most.

| App | Mode | Infer (edit) | Before | After | / infer after | p99 before -> after | max before -> after | constraints | moves | liveness |
|---|---|---|---|---|---|---|---|---|---|---|
| ripgrep | polonius | 610 | 401 | 325 | 53% | 1435 -> 1137 | 7044 -> 7194 | 163 -> 126 | 78 -> 43 | 68 -> 64 |
| ripgrep | nll | 610 | 356 | 280 | 46% | 1320 -> 1022 | 6733 -> 6729 | 147 -> 110 | 71 -> 38 | 63 -> 59 |
| fd | polonius | 190 | 105 | 89 | 47% | 8745 -> 8280 | 15313 -> 15563 | 44 -> 35 | 17 -> 10 | 19 -> 18 |
| fd | nll | 190 | 97 | 81 | 43% | 8575 -> 8098 | 14909 -> 14969 | 41 -> 33 | 16 -> 10 | 18 -> 17 |
| bat | polonius | 278 | 227 | 197 | 71% | 3195 -> 2981 | 63350 -> 62759 | 76 -> 62 | 32 -> 19 | 50 -> 47 |
| bat | nll | 278 | 209 | 180 | 65% | 3001 -> 2706 | 62941 -> 62778 | 70 -> 55 | 30 -> 18 | 47 -> 44 |
| hyperfine | polonius | 206 | 118 | 81 | 39% | 5747 -> 3807 | 13049 -> 8451 | 62 -> 38 | 23 -> 8 | 14 -> 13 |
| hyperfine | nll | 206 | 108 | 71 | 35% | 5472 -> 3535 | 12162 -> 7469 | 57 -> 35 | 21 -> 8 | 13 -> 12 |
| just | polonius | 1076 | 703 | 602 | 56% | 8595 -> 7148 | 19471 -> 18868 | 301 -> 244 | 102 -> 62 | 146 -> 142 |
| just | nll | 1076 | 652 | 546 | 51% | 8044 -> 6793 | 18647 -> 17842 | 285 -> 224 | 97 -> 58 | 141 -> 136 |
| axum | polonius | 3887 | 2575 | 1699 | 44% | 32816 -> 19302 | 50744 -> 31971 | 1137 -> 490 | 791 -> 105 | 321 -> 290 |
| axum | nll | 3887 | 2510 | 1642 | 42% | 32349 -> 19011 | 50430 -> 31696 | 1119 -> 475 | 784 -> 102 | 316 -> 284 |
| helix | polonius | 3130 | 1852 | 1509 | 48% | 2783 -> 2181 | 110190 -> 110520 | 794 -> 617 | 340 -> 181 | 345 -> 321 |
| helix | nll | 3130 | 1683 | 1334 | 43% | 2605 -> 2041 | 109087 -> 104855 | 734 -> 553 | 318 -> 165 | 331 -> 301 |
| nushell | polonius | 14709 | 11581 | 9686 | 66% | 2944 -> 2486 | 222300 -> 202434 | 4187 -> 3167 | 1668 -> 823 | 2579 -> 2191 |
| nushell | nll | 14709 | 10727 | 8842 | 60% | 2806 -> 2353 | 226457 -> 202500 | 3878 -> 2860 | 1572 -> 756 | 2443 -> 2065 |

A body's cold first check, in ms: ripgrep 877 -> 878, fd 1236 -> 1262, bat 504 -> 523,
hyperfine 247 -> 230, just 3470 -> 3569, axum 6713 -> 8299, helix 3758 -> 3811,
nushell 26866 -> 27123.
