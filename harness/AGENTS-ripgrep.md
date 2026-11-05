# Agent-style edits: ripgrep, seed 1

80 of 80 steps right, 0 restarts, 76 patches (median 327 ms with the link), 0 full objects, rss 1311 MB at the end. Warmup 11.1 s.

| # | Edit | Reply | ms | Result |
|---|---|---|---:|---|
| 0 | unit 0 added (enum) | patch | 352 | same |
| 1 | unit 1 added (macro) | patch | 319 | same |
| 2 | unit 2 added (async) | patch | 291 | same |
| 3 | unit 1 (macro) to version 1 | patch | 290 | same |
| 4 | unit 0 (enum) to version 1 | patch | 435 | same |
| 5 | unit 3 added (async) | patch | 327 | same |
| 6 | unit 4 added (closure) | patch | 409 | same |
| 7 | unit 4 (closure) to version 1 | patch | 401 | same |
| 8 | unit 5 added (async) | patch | 253 | same |
| 9 | BROKEN: syntax error in crates/core/blaze_u3.rs | error | 56 | said so |
| 10 | mended | patch | 268 | same |
| 11 | unit 6 added (static) | patch | 279 | same |
| 12 | unit 7 added (plain) | patch | 304 | same |
| 13 | unit 8 added (collections) | patch | 400 | same |
| 14 | unit 9 added (closure) | patch | 305 | same |
| 15 | blank in crates/printer/src/standard.rs | patch | 331 | same |
| 16 | library function 11 added | patch | 336 | same |
| 17 | burst: library function 11 to version 1; unit 3 (async) to version 1; library function 12 added; unit 13 added (closure); unit 14 added (trait) | patch | 331 | same |
| 18 | unit 1 (macro) to version 2 | patch | 302 | same |
| 19 | unit 14 (trait) to version 1 | patch | 409 | same |
| 20 | library function 15 added | patch | 341 | same |
| 21 | unit 8 (collections) to version 1 | patch | 310 | same |
| 22 | let in crates/searcher/src/searcher/core.rs | patch | 323 | same |
| 23 | unit 8 (collections) removed | patch | 355 | same |
| 24 | unit 13 (closure) to version 1 | patch | 253 | same |
| 25 | back to the first source | patch | 383 | same |
| 26 | forward again | patch | 509 | same |
| 27 | unit 17 added (async) | patch | 392 | same |
| 28 | burst: unit 14 (trait) removed; let in crates/searcher/src/searcher/core.rs; library function 19 added; unit 20 added (trait); unit 5 (async) to version 1 | patch | 298 | same |
| 29 | unit 20 (trait) removed | patch | 319 | same |
| 30 | unit 21 added (async) | patch | 399 | same |
| 31 | unit 22 added (macro) | patch | 352 | same |
| 32 | unit 17 (async) to version 1 | patch | 337 | same |
| 33 | comment in crates/regex/src/literal.rs | patch | 255 | same |
| 34 | unit 24 added (generic) | patch | 298 | same |
| 35 | library function 25 added | patch | 319 | same |
| 36 | library function 25 to version 1 | patch | 346 | same |
| 37 | unit 24 (generic) to version 1 | patch | 338 | same |
| 38 | unit 7 (plain) to version 1 | patch | 285 | same |
| 39 | BROKEN: unresolved name in crates/core/blaze_u24.rs | error | 71 | said so |
| 40 | mended | patch | 250 | same |
| 41 | unit 26 added (closure) | patch | 396 | same |
| 42 | library function 27 added | patch | 371 | same |
| 43 | library function 28 added | patch | 321 | same |
| 44 | blank in crates/printer/src/standard.rs | patch | 268 | same |
| 45 | unit 24 (generic) removed | patch | 320 | same |
| 46 | unit 13 (closure) to version 2 | patch | 247 | same |
| 47 | library function 25 to version 2 | patch | 314 | same |
| 48 | unit 7 (plain) to version 2 | patch | 250 | same |
| 49 | unit 6 (static) to version 1 | patch | 257 | same |
| 50 | back to the first source | patch | 284 | same |
| 51 | forward again | patch | 358 | same |
| 52 | unit 0 (enum) to version 2 | patch | 310 | same |
| 53 | library function 28 to version 1 | patch | 291 | same |
| 54 | unit 7 (plain) to version 3 | patch | 254 | same |
| 55 | unit 30 added (generic) | patch | 338 | same |
| 56 | comment in crates/ignore/src/walk.rs | patch | 344 | same |
| 57 | unit 32 added (collections) | patch | 438 | same |
| 58 | BROKEN: unresolved name in crates/regex/src/literal.rs | error | 112 | said so |
| 59 | mended | patch | 373 | same |
| 60 | unit 33 added (struct) | patch | 305 | same |
| 61 | unit 34 added (async) | patch | 385 | same |
| 62 | unit 35 added (macro) | patch | 327 | same |
| 63 | burst: unit 36 added (enum); let in crates/searcher/src/searcher/core.rs; let in crates/regex/src/literal.rs; let in crates/globset/src/glob.rs | patch | 384 | same |
| 64 | unit 30 (generic) to version 1 | patch | 300 | same |
| 65 | unit 32 (collections) to version 1 | patch | 334 | same |
| 66 | unit 3 (async) removed | patch | 296 | same |
| 67 | unit 40 added (macro) | patch | 455 | same |
| 68 | unit 21 (async) to version 1 | patch | 313 | same |
| 69 | unit 7 (plain) to version 4 | patch | 283 | same |
| 70 | BROKEN: type error in crates/printer/src/standard.rs | error | 104 | said so |
| 71 | mended | patch | 377 | same |
| 72 | library function 41 added | patch | 326 | same |
| 73 | unit 0 (enum) to version 3 | patch | 291 | same |
| 74 | burst: unit 5 (async) to version 2; unit 42 added (trait) | patch | 364 | same |
| 75 | back to the first source | patch | 344 | same |
| 76 | forward again | patch | 488 | same |
| 77 | unit 34 (async) to version 1 | patch | 414 | same |
| 78 | unit 35 (macro) to version 1 | patch | 492 | same |
| 79 | burst: unit 34 (async) to version 2; unit 43 added (closure); unit 43 (closure) to version 1; unit 1 (macro) removed; unit 6 (static) to version 2 | patch | 598 | same |
