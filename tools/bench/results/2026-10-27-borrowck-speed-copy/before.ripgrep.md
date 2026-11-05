| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| ripgrep | polonius | 2980 | 2205 | 615 | 401 | 0 | 65.3% | 877 | 39 | 1435 | 7044 |
| ripgrep | nll | 2980 | 2205 | 615 | 356 | 0 | 57.9% | 877 | 34 | 1320 | 6733 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 163 | 7 | 2 | 9 | 68 | 6 | 0 | 79 | 11 | 1 | 28 |
| nll | 148 | 6 | 1 | 9 | 63 | 5 | 0 | 72 | 1 | 0 | 27 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 7044 | 6130 | ripgrep | `ignore::default_types::DEFAULT_TYPES` | 10832 | 10683 | 0 | 10305 | 10305 | constraints 2553, liveness 1020, moves 905 |
| 3005 | 4516 | ripgrep | `rg::flags::lowargs::<LowArgs>::fmt` | 2498 | 2573 | 234 | 3813 | 3813 | constraints 1000, liveness 707, moves 361 |
| 3003 | 3654 | ripgrep | `rg::flags::hiargs::<HiArgs>::from_low_args` | 1715 | 365 | 52 | 322 | 322 | constraints 1102, liveness 696, moves 611 |
| 2748 | 4786 | ripgrep | `ignore::dir::<Ignore>::matched_ignore` | 1130 | 548 | 24 | 662 | 662 | constraints 1415, liveness 462, moves 393 |
| 2737 | 4149 | ripgrep | `rg::flags::hiargs::<HiArgs>::fmt` | 2274 | 2342 | 213 | 3470 | 3470 | constraints 925, liveness 627, moves 328 |
| 2732 | 5020 | ripgrep | `rg::search_parallel` | 1342 | 488 | 58 | 464 | 464 | constraints 910, liveness 603, moves 562 |
| 2542 | 5357 | ripgrep | `ignore::dir::<Ignore>::add_child_path_with_found_ignore_files` | 911 | 465 | 19 | 463 | 461 | constraints 1405, liveness 366, moves 354 |
| 2465 | 4996 | ripgrep | `ignore::walk::<WalkParallel>::visit` | 817 | 340 | 30 | 401 | 397 | constraints 1132, liveness 496, moves 465 |
| 2462 | 4416 | ripgrep | `rg::flags::doc::help::generate_long_flag` | 1286 | 815 | 57 | 812 | 812 | constraints 1137, moves 405, liveness 384 |
| 2245 | 3352 | ripgrep | `globset::<GlobSet>::new` | 1164 | 365 | 54 | 309 | 309 | constraints 791, liveness 531, moves 459 |
| 2033 | 1951 | ripgrep | `rg::search` | 886 | 212 | 27 | 190 | 190 | constraints 664, liveness 512, moves 376 |
| 2002 | 3997 | ripgrep | `grep_printer::standard::<StandardImpl<'a, M, W>>::write_exceeded_line` | 1022 | 367 | 14 | 381 | 381 | constraints 1025, moves 329, liveness 317 |
| 1840 | 2456 | ripgrep | `grep_regex::config::<ConfiguredHIR>::new` | 988 | 310 | 40 | 282 | 282 | constraints 632, liveness 452, moves 408 |
| 1762 | 3152 | ripgrep | `rg::flags::parse::<Parser>::parse` | 908 | 311 | 22 | 287 | 287 | constraints 702, moves 411, liveness 325 |
| 1666 | 3149 | ripgrep | `grep_regex::ban::check` | 818 | 290 | 18 | 289 | 281 | constraints 832, moves 302, liveness 235 |
| 1623 | 1700 | ripgrep | `rg::flags::hiargs::<HiArgs>::walk_builder` | 801 | 263 | 35 | 241 | 241 | constraints 577, liveness 381, moves 276 |
| 1615 | 2466 | ripgrep | `ignore::types::<TypesBuilder>::build` | 645 | 164 | 14 | 156 | 156 | constraints 678, liveness 355, moves 320 |
| 1602 | 3638 | ripgrep | `rg::flags::doc::man::generate_flag` | 1052 | 706 | 49 | 683 | 683 | constraints 697, moves 323, liveness 229 |
| 1556 | 2208 | ripgrep | `grep_searcher::searcher::<Searcher>::search_reader` | 591 | 262 | 31 | 247 | 247 | constraints 698, moves 282, liveness 213 |
| 1545 | 2315 | ripgrep | `grep_regex::strip::strip_from_match_ascii` | 827 | 143 | 19 | 132 | 132 | constraints 692, moves 300, liveness 270 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 6733 | 6130 | ripgrep | `ignore::default_types::DEFAULT_TYPES` | 10832 | 10683 | 0 | 10305 | 10305 | constraints 2465, infer 1087, liveness 963 |
| 2865 | 3654 | ripgrep | `rg::flags::hiargs::<HiArgs>::from_low_args` | 1715 | 365 | 52 | 322 | 322 | constraints 1030, liveness 698, moves 602 |
| 2644 | 4516 | ripgrep | `rg::flags::lowargs::<LowArgs>::fmt` | 2498 | 2573 | 234 | 3813 | 3813 | constraints 935, liveness 701, moves 359 |
| 2615 | 5357 | ripgrep | `ignore::dir::<Ignore>::add_child_path_with_found_ignore_files` | 911 | 465 | 19 | 463 | 461 | constraints 1571, liveness 360, moves 331 |
| 2577 | 5020 | ripgrep | `rg::search_parallel` | 1342 | 488 | 58 | 464 | 464 | constraints 865, liveness 592, moves 539 |
| 2571 | 4786 | ripgrep | `ignore::dir::<Ignore>::matched_ignore` | 1130 | 548 | 24 | 662 | 662 | constraints 1328, liveness 448, moves 377 |
| 2405 | 4149 | ripgrep | `rg::flags::hiargs::<HiArgs>::fmt` | 2274 | 2342 | 213 | 3470 | 3470 | constraints 870, liveness 623, moves 325 |
| 2325 | 4996 | ripgrep | `ignore::walk::<WalkParallel>::visit` | 817 | 340 | 30 | 401 | 397 | constraints 1043, liveness 519, moves 427 |
| 2312 | 4416 | ripgrep | `rg::flags::doc::help::generate_long_flag` | 1286 | 815 | 57 | 812 | 812 | constraints 1081, moves 401, liveness 369 |
| 2098 | 3352 | ripgrep | `globset::<GlobSet>::new` | 1164 | 365 | 54 | 309 | 309 | constraints 760, liveness 497, moves 450 |
| 1889 | 3997 | ripgrep | `grep_printer::standard::<StandardImpl<'a, M, W>>::write_exceeded_line` | 1022 | 367 | 14 | 381 | 381 | constraints 1004, moves 328, liveness 273 |
| 1887 | 1951 | ripgrep | `rg::search` | 886 | 212 | 27 | 190 | 190 | constraints 641, liveness 490, moves 343 |
| 1743 | 2456 | ripgrep | `grep_regex::config::<ConfiguredHIR>::new` | 988 | 310 | 40 | 282 | 282 | constraints 607, liveness 459, moves 380 |
| 1582 | 3152 | ripgrep | `rg::flags::parse::<Parser>::parse` | 908 | 311 | 22 | 287 | 287 | constraints 600, moves 387, liveness 324 |
| 1541 | 3149 | ripgrep | `grep_regex::ban::check` | 818 | 290 | 18 | 289 | 281 | constraints 760, moves 293, liveness 225 |
| 1506 | 3638 | ripgrep | `rg::flags::doc::man::generate_flag` | 1052 | 706 | 49 | 683 | 683 | constraints 669, moves 315, liveness 217 |
| 1478 | 2315 | ripgrep | `grep_regex::strip::strip_from_match_ascii` | 827 | 143 | 19 | 132 | 132 | constraints 676, moves 290, liveness 272 |
| 1475 | 2466 | ripgrep | `ignore::types::<TypesBuilder>::build` | 645 | 164 | 14 | 156 | 156 | constraints 658, liveness 317, moves 283 |
| 1442 | 2424 | ripgrep | `ignore::incremental::<IncrementalIgnore>::matched_with_errors_ignore` | 730 | 222 | 29 | 243 | 243 | constraints 590, liveness 338, moves 241 |
| 1438 | 1700 | ripgrep | `rg::flags::hiargs::<HiArgs>::walk_builder` | 801 | 263 | 35 | 241 | 241 | constraints 530, liveness 383, moves 261 |
