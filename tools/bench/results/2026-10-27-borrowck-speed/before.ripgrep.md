| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| ripgrep | polonius | 2980 | 2466 | 639 | 468 | 0 | 73.2% | 971 | 41 | 1592 | 7902 |
| ripgrep | nll | 2980 | 2466 | 639 | 419 | 0 | 65.6% | 971 | 36 | 1478 | 7010 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 223 | 7 | 2 | 9 | 71 | 6 | 0 | 81 | 12 | 1 | 29 |
| nll | 206 | 7 | 2 | 9 | 64 | 5 | 0 | 73 | 1 | 1 | 27 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 7902 | 6322 | ripgrep | `ignore::default_types::DEFAULT_TYPES` | 10832 | 10683 | 0 | 10305 | 10305 | constraints 2893, liveness 1343, moves 948 |
| 3695 | 4893 | ripgrep | `rg::flags::lowargs::<LowArgs>::fmt` | 2498 | 2573 | 234 | 3813 | 3813 | constraints 1630, liveness 727, moves 374 |
| 3609 | 3858 | ripgrep | `rg::flags::hiargs::<HiArgs>::from_low_args` | 1715 | 365 | 52 | 322 | 322 | constraints 1554, liveness 773, moves 648 |
| 3362 | 4397 | ripgrep | `rg::flags::hiargs::<HiArgs>::fmt` | 2274 | 2342 | 213 | 3470 | 3470 | constraints 1493, liveness 631, moves 342 |
| 3121 | 5458 | ripgrep | `rg::search_parallel` | 1342 | 488 | 58 | 464 | 464 | constraints 1205, liveness 623, moves 574 |
| 2886 | 4553 | ripgrep | `rg::flags::doc::help::generate_long_flag` | 1286 | 815 | 57 | 812 | 812 | constraints 1453, liveness 459, moves 410 |
| 2805 | 5015 | ripgrep | `ignore::dir::<Ignore>::matched_ignore` | 1130 | 548 | 24 | 662 | 662 | constraints 1487, liveness 476, moves 379 |
| 2700 | 5350 | ripgrep | `ignore::walk::<WalkParallel>::visit` | 817 | 340 | 30 | 401 | 397 | constraints 1332, liveness 518, moves 467 |
| 2655 | 3495 | ripgrep | `globset::<GlobSet>::new` | 1164 | 365 | 54 | 309 | 309 | constraints 1168, liveness 544, moves 466 |
| 2639 | 5401 | ripgrep | `ignore::dir::<Ignore>::add_child_path_with_found_ignore_files` | 911 | 465 | 19 | 463 | 461 | constraints 1555, liveness 363, moves 334 |
| 2430 | 2121 | ripgrep | `rg::search` | 886 | 212 | 27 | 190 | 190 | constraints 957, liveness 617, moves 378 |
| 2430 | 3259 | ripgrep | `rg::flags::parse::<Parser>::parse` | 908 | 311 | 22 | 287 | 287 | constraints 1287, moves 418, liveness 384 |
| 2344 | 4122 | ripgrep | `grep_printer::standard::<StandardImpl<'a, M, W>>::write_exceeded_line` | 1022 | 367 | 14 | 381 | 381 | constraints 1316, moves 359, liveness 349 |
| 2164 | 2546 | ripgrep | `grep_regex::config::<ConfiguredHIR>::new` | 988 | 310 | 40 | 282 | 282 | constraints 927, liveness 471, moves 410 |
| 2099 | 2357 | ripgrep | `grep_searcher::searcher::<Searcher>::search_reader` | 591 | 262 | 31 | 247 | 247 | constraints 1175, moves 292, liveness 249 |
| 1979 | 3842 | ripgrep | `rg::flags::doc::man::generate_flag` | 1052 | 706 | 49 | 683 | 683 | constraints 1009, moves 334, liveness 260 |
| 1824 | 2309 | ripgrep | `walk::main` | 651 | 129 | 22 | 100 | 100 | constraints 712, liveness 498, moves 309 |
| 1819 | 2595 | ripgrep | `ignore::types::<TypesBuilder>::build` | 645 | 164 | 14 | 156 | 156 | constraints 884, liveness 354, moves 313 |
| 1762 | 3330 | ripgrep | `grep_regex::ban::check` | 818 | 286 | 18 | 289 | 281 | constraints 902, moves 303, liveness 249 |
| 1736 | 2604 | ripgrep | `rg::flags::defs::FLAGS` | 1523 | 879 | 0 | 879 | 879 | constraints 544, liveness 496, loans 224 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 7010 | 6322 | ripgrep | `ignore::default_types::DEFAULT_TYPES` | 10832 | 10683 | 0 | 10305 | 10305 | constraints 2665, infer 1099, liveness 996 |
| 3430 | 3858 | ripgrep | `rg::flags::hiargs::<HiArgs>::from_low_args` | 1715 | 365 | 52 | 322 | 322 | constraints 1479, liveness 720, moves 677 |
| 3286 | 4893 | ripgrep | `rg::flags::lowargs::<LowArgs>::fmt` | 2498 | 2573 | 234 | 3813 | 3813 | constraints 1467, liveness 746, moves 367 |
| 3102 | 4397 | ripgrep | `rg::flags::hiargs::<HiArgs>::fmt` | 2274 | 2342 | 213 | 3470 | 3470 | constraints 1467, liveness 663, moves 367 |
| 2994 | 5458 | ripgrep | `rg::search_parallel` | 1342 | 488 | 58 | 464 | 464 | constraints 1172, liveness 638, moves 585 |
| 2693 | 5015 | ripgrep | `ignore::dir::<Ignore>::matched_ignore` | 1130 | 548 | 24 | 662 | 662 | constraints 1433, liveness 473, moves 374 |
| 2674 | 4553 | ripgrep | `rg::flags::doc::help::generate_long_flag` | 1286 | 815 | 57 | 812 | 812 | constraints 1395, liveness 414, moves 405 |
| 2645 | 5350 | ripgrep | `ignore::walk::<WalkParallel>::visit` | 817 | 340 | 30 | 401 | 397 | constraints 1375, liveness 489, moves 431 |
| 2600 | 5401 | ripgrep | `ignore::dir::<Ignore>::add_child_path_with_found_ignore_files` | 911 | 465 | 19 | 463 | 461 | constraints 1528, liveness 388, moves 339 |
| 2581 | 3495 | ripgrep | `globset::<GlobSet>::new` | 1164 | 365 | 54 | 309 | 309 | constraints 1174, liveness 533, moves 475 |
| 2328 | 3259 | ripgrep | `rg::flags::parse::<Parser>::parse` | 908 | 311 | 22 | 287 | 287 | constraints 1244, moves 420, liveness 371 |
| 2202 | 2121 | ripgrep | `rg::search` | 886 | 212 | 27 | 190 | 190 | constraints 883, liveness 528, moves 358 |
| 2189 | 4122 | ripgrep | `grep_printer::standard::<StandardImpl<'a, M, W>>::write_exceeded_line` | 1022 | 367 | 14 | 381 | 381 | constraints 1300, moves 336, liveness 280 |
| 2038 | 2546 | ripgrep | `grep_regex::config::<ConfiguredHIR>::new` | 988 | 310 | 40 | 282 | 282 | constraints 924, liveness 424, moves 393 |
| 1911 | 2357 | ripgrep | `grep_searcher::searcher::<Searcher>::search_reader` | 591 | 262 | 31 | 247 | 247 | constraints 1109, moves 278, liveness 219 |
| 1816 | 3842 | ripgrep | `rg::flags::doc::man::generate_flag` | 1052 | 706 | 49 | 683 | 683 | constraints 970, moves 318, liveness 218 |
| 1678 | 2595 | ripgrep | `ignore::types::<TypesBuilder>::build` | 645 | 164 | 14 | 156 | 156 | constraints 826, liveness 336, moves 290 |
| 1641 | 1821 | ripgrep | `rg::flags::hiargs::<HiArgs>::walk_builder` | 801 | 263 | 35 | 241 | 241 | constraints 661, liveness 426, moves 275 |
| 1635 | 2508 | ripgrep | `grep_regex::strip::strip_from_match_ascii` | 827 | 143 | 19 | 132 | 132 | constraints 808, moves 292, liveness 261 |
| 1617 | 3330 | ripgrep | `grep_regex::ban::check` | 818 | 286 | 18 | 289 | 281 | constraints 852, moves 286, liveness 231 |
