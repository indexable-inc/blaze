# Agent-style edits: just, seed 1

80 of 80 steps right, 0 restarts, 77 patches (median 484 ms with the link), 0 full objects, rss 2083 MB at the end. Warmup 16.6 s.

| # | Edit | Reply | ms | Result |
|---|---|---|---:|---|
| 0 | unit 0 added (enum) | patch | 734 | same |
| 1 | unit 1 added (macro) | patch | 591 | same |
| 2 | unit 2 added (async) | patch | 602 | same |
| 3 | unit 1 (macro) to version 1 | patch | 570 | same |
| 4 | unit 0 (enum) to version 1 | patch | 576 | same |
| 5 | unit 3 added (async) | patch | 418 | same |
| 6 | unit 4 added (closure) | patch | 488 | same |
| 7 | unit 4 (closure) to version 1 | patch | 487 | same |
| 8 | unit 5 added (async) | patch | 464 | same |
| 9 | BROKEN: unresolved name in src/compiler.rs | error | 98 | said so |
| 10 | mended | patch | 474 | same |
| 11 | unit 6 added (static) | patch | 449 | same |
| 12 | unit 7 added (plain) | patch | 464 | same |
| 13 | unit 8 added (collections) | patch | 603 | same |
| 14 | unit 9 added (closure) | patch | 504 | same |
| 15 | blank in src/compiler.rs | patch | 365 | same |
| 16 | burst: unit 8 (collections) to version 1; unit 11 added (static); unit 12 added (generic); unit 7 (plain) to version 1; fn in src/main.rs | patch | 493 | same |
| 17 | unit 14 added (trait) | patch | 617 | same |
| 18 | unit 1 (macro) to version 2 | patch | 411 | same |
| 19 | unit 12 (generic) to version 1 | patch | 503 | same |
| 20 | unit 8 (collections) to version 2 | patch | 433 | same |
| 21 | let in src/lib.rs | patch | 411 | same |
| 22 | unit 9 (closure) to version 1 | patch | 489 | same |
| 23 | unit 16 added (async) | patch | 656 | same |
| 24 | burst: let in src/run.rs; unit 18 added (async); unit 8 (collections) to version 3; unit 19 added (trait); unit 14 (trait) to version 1 | patch | 470 | same |
| 25 | back to the first source | patch | 541 | same |
| 26 | forward again | patch | 648 | same |
| 27 | unit 19 (trait) to version 1 | patch | 486 | same |
| 28 | unit 20 added (async) | patch | 474 | same |
| 29 | unit 21 added (macro) | patch | 484 | same |
| 30 | unit 5 (async) to version 1 | patch | 578 | same |
| 31 | comment in src/config.rs | patch | 374 | same |
| 32 | unit 23 added (generic) | patch | 529 | same |
| 33 | burst: unit 12 (generic) to version 2; unit 23 (generic) removed; let in src/compiler.rs; unit 0 (enum) to version 2; comment in src/config.rs | patch | 526 | same |
| 34 | blank in src/compiler.rs | patch | 424 | same |
| 35 | unit 12 (generic) removed | patch | 522 | same |
| 36 | fn in src/config.rs | patch | 483 | same |
| 37 | unit 14 (trait) removed | patch | 597 | same |
| 38 | unit 16 (async) to version 1 | patch | 433 | same |
| 39 | unit 28 added (collections) | patch | 495 | same |
| 40 | unit 20 (async) to version 1 | patch | 1393 | same |
| 41 | unit 29 added (generic) | patch | 451 | same |
| 42 | comment in src/config.rs | patch | 403 | same |
| 43 | unit 31 added (collections) | patch | 493 | same |
| 44 | BROKEN: unresolved name in src/run.rs | error | 95 | said so |
| 45 | mended | patch | 395 | same |
| 46 | unit 32 added (struct) | patch | 451 | same |
| 47 | unit 33 added (async) | patch | 454 | same |
| 48 | unit 34 added (macro) | patch | 440 | same |
| 49 | burst: unit 35 added (enum); unit 36 added (static); comment in src/main.rs; unit 38 added (macro) | patch | 475 | same |
| 50 | back to the first source | patch | 384 | same |
| 51 | forward again | patch | 668 | same |
| 52 | let in src/lib.rs | patch | 450 | same |
| 53 | unit 21 (macro) to version 1 | patch | 485 | same |
| 54 | unit 3 (async) removed | patch | 445 | same |
| 55 | unit 40 added (macro) | patch | 415 | same |
| 56 | unit 16 (async) to version 2 | patch | 459 | same |
| 57 | unit 7 (plain) to version 2 | patch | 449 | same |
| 58 | BROKEN: type error in src/compiler.rs | error | 99 | said so |
| 59 | mended | patch | 483 | same |
| 60 | fn in src/config.rs | patch | 482 | same |
| 61 | unit 42 added (generic) | patch | 486 | same |
| 62 | unit 43 added (closure) | patch | 482 | same |
| 63 | comment in src/main.rs | patch | 493 | same |
| 64 | unit 36 (static) to version 1 | patch | 462 | same |
| 65 | unit 31 (collections) to version 1 | patch | 405 | same |
| 66 | burst: unit 45 added (collections); unit 0 (enum) to version 3; let in src/config.rs; unit 20 (async) to version 2; unit 47 added (macro) | patch | 530 | same |
| 67 | blank in src/run.rs | patch | 373 | same |
| 68 | unit 2 (async) to version 1 | patch | 433 | same |
| 69 | unit 49 added (macro) | patch | 455 | same |
| 70 | unit 38 (macro) to version 1 | patch | 372 | same |
| 71 | let in src/compiler.rs | patch | 437 | same |
| 72 | unit 51 added (collections) | patch | 563 | same |
| 73 | unit 52 added (enum) | patch | 706 | same |
| 74 | fn in src/config.rs | patch | 579 | same |
| 75 | back to the first source | patch | 512 | same |
| 76 | forward again | patch | 918 | same |
| 77 | blank in src/config.rs | patch | 561 | same |
| 78 | unit 19 (trait) to version 2 | patch | 574 | same |
| 79 | unit 7 (plain) to version 3 | patch | 526 | same |
