# The borrow checker's time on the bench apps, before and after a trait reference is proven by a query

Made by `harness/borrowck_speed.py --jobs 1` (one `blaze-engine borrowck --project DIR --timings`
per app, see docs/borrowck.md "Speed"), in an arm64 ix VM (32 vCPUs, 32 GiB), inside
`nix develop`. `before.*` is main a4c6dd3c with #576 (timings as an edit pays them, inferred
outlives a query, `is_copy` once per type); `after.*` is main 3db5b233 with the trait reference
proven by a canonical salsa query. Each `.json` is the harness's summary, each `.md` its tables
with the slowest bodies.

Times in ms, the checker without MIR lowering as an edit of each body pays it, beside an edit's
type inference; the slowest body in us; the phases that moved most.

| App | Mode | Infer (edit) | Before | After | / infer after | max before -> after | constraints | moves | liveness |
|---|---|---|---|---|---|---|---|---|---|
| ripgrep | polonius | 630 | 467 | 409 | 65% | 7902 -> 8377 | 223 -> 167 | 80 -> 79 | 71 -> 69 |
| ripgrep | nll | 630 | 418 | 358 | 57% | 7010 -> 7795 | 206 -> 148 | 73 -> 72 | 64 -> 63 |
| fd | polonius | 199 | 118 | 111 | 56% | 16470 -> 17642 | 56 -> 48 | 18 -> 18 | 19 -> 19 |
| fd | nll | 199 | 109 | 101 | 51% | 15748 -> 16622 | 53 -> 44 | 16 -> 16 | 18 -> 18 |
| bat | polonius | 289 | 255 | 234 | 81% | 66175 -> 64304 | 100 -> 81 | 33 -> 33 | 51 -> 50 |
| bat | nll | 289 | 236 | 214 | 74% | 64764 -> 63947 | 94 -> 73 | 30 -> 30 | 48 -> 47 |
| hyperfine | polonius | 223 | 148 | 124 | 56% | 15821 -> 14849 | 89 -> 65 | 24 -> 24 | 14 -> 14 |
| hyperfine | nll | 223 | 139 | 113 | 51% | 15505 -> 13929 | 86 -> 61 | 22 -> 22 | 14 -> 13 |
| just | polonius | 1070 | 845 | 715 | 67% | 27804 -> 20569 | 418 -> 306 | 110 -> 103 | 157 -> 148 |
| just | nll | 1070 | 781 | 659 | 62% | 26239 -> 19366 | 395 -> 287 | 103 -> 97 | 149 -> 142 |
| axum | polonius | 3921 | 3535 | 2594 | 66% | 73929 -> 51802 | 2035 -> 1146 | 820 -> 788 | 336 -> 329 |
| axum | nll | 3921 | 3464 | 2519 | 64% | 74334 -> 51391 | 2013 -> 1124 | 813 -> 778 | 330 -> 322 |
| helix | polonius | 3183 | 2335 | 1899 | 60% | 124844 -> 123970 | 1199 -> 818 | 373 -> 346 | 369 -> 357 |
| helix | nll | 3183 | 2126 | 1709 | 54% | 122550 -> 117617 | 1111 -> 747 | 347 -> 323 | 351 -> 336 |
| nushell | polonius | 15613 | 19134 | 11980 | 77% | 811113 -> 239613 | 10747 -> 4371 | 1778 -> 1734 | 2827 -> 2670 |
| nushell | nll | 15613 | 17614 | 10952 | 70% | 811335 -> 224408 | 10329 -> 3982 | 1650 -> 1599 | 2675 -> 2500 |

A body's cold first check, which proves the goals no body proved before it, in ms:
ripgrep 971 -> 967, fd 1331 -> 1303, bat 550 -> 545, hyperfine 263 -> 272, just 3833 -> 3594,
axum 5176 -> 6740, helix 4661 -> 4045, nushell 31654 -> 28852.
