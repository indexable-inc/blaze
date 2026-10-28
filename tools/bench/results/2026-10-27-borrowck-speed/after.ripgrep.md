| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| ripgrep | polonius | 2980 | 2409 | 630 | 409 | 0 | 65.0% | 967 | 40 | 1497 | 8377 |
| ripgrep | nll | 2980 | 2409 | 630 | 359 | 0 | 57.0% | 967 | 34 | 1310 | 7795 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 167 | 7 | 2 | 9 | 70 | 6 | 0 | 80 | 11 | 1 | 28 |
| nll | 148 | 7 | 2 | 9 | 64 | 5 | 0 | 73 | 1 | 0 | 27 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 8377 | 6365 | ripgrep | `ignore::default_types::DEFAULT_TYPES` | 10832 | 10683 | 0 | 10305 | 10305 | constraints 3364, liveness 1274, moves 997 |
| 3254 | 3938 | ripgrep | `rg::flags::hiargs::<HiArgs>::from_low_args` | 1715 | 365 | 52 | 322 | 322 | constraints 1206, liveness 721, moves 668 |
| 3238 | 4643 | ripgrep | `rg::flags::lowargs::<LowArgs>::fmt` | 2498 | 2573 | 234 | 3813 | 3813 | constraints 1153, liveness 731, moves 372 |
| 3053 | 4321 | ripgrep | `rg::flags::hiargs::<HiArgs>::fmt` | 2274 | 2342 | 213 | 3470 | 3470 | constraints 1060, liveness 757, moves 358 |
| 2962 | 5494 | ripgrep | `ignore::dir::<Ignore>::matched_ignore` | 1130 | 548 | 24 | 662 | 662 | constraints 1587, liveness 481, moves 402 |
| 2961 | 6058 | ripgrep | `rg::search_parallel` | 1342 | 488 | 58 | 464 | 464 | constraints 1055, liveness 657, moves 572 |
| 2674 | 5798 | ripgrep | `ignore::walk::<WalkParallel>::visit` | 817 | 340 | 30 | 401 | 397 | constraints 1278, liveness 548, moves 459 |
| 2652 | 5536 | ripgrep | `ignore::dir::<Ignore>::add_child_path_with_found_ignore_files` | 911 | 465 | 19 | 463 | 461 | constraints 1478, liveness 395, moves 366 |
| 2635 | 4602 | ripgrep | `rg::flags::doc::help::generate_long_flag` | 1286 | 815 | 57 | 812 | 812 | constraints 1255, liveness 433, moves 415 |
| 2423 | 3512 | ripgrep | `globset::<GlobSet>::new` | 1164 | 365 | 54 | 309 | 309 | constraints 894, liveness 556, moves 486 |
| 2276 | 2157 | ripgrep | `rg::search` | 886 | 212 | 27 | 190 | 190 | constraints 777, liveness 578, moves 410 |
| 2073 | 4228 | ripgrep | `grep_printer::standard::<StandardImpl<'a, M, W>>::write_exceeded_line` | 1022 | 367 | 14 | 381 | 381 | constraints 1098, moves 349, liveness 295 |
| 1923 | 2531 | ripgrep | `grep_regex::config::<ConfiguredHIR>::new` | 988 | 310 | 40 | 282 | 282 | constraints 686, liveness 465, moves 408 |
| 1840 | 3220 | ripgrep | `rg::flags::parse::<Parser>::parse` | 908 | 311 | 22 | 287 | 287 | constraints 709, moves 424, liveness 361 |
| 1744 | 3447 | ripgrep | `grep_regex::ban::check` | 818 | 290 | 18 | 289 | 281 | constraints 881, moves 316, liveness 240 |
| 1693 | 3964 | ripgrep | `rg::flags::doc::man::generate_flag` | 1052 | 706 | 49 | 683 | 683 | constraints 739, moves 337, liveness 252 |
| 1678 | 1848 | ripgrep | `rg::flags::hiargs::<HiArgs>::walk_builder` | 801 | 263 | 35 | 241 | 241 | constraints 614, liveness 413, moves 282 |
| 1644 | 2342 | ripgrep | `grep_searcher::searcher::<Searcher>::search_reader` | 591 | 262 | 31 | 247 | 247 | constraints 729, moves 308, liveness 217 |
| 1633 | 2533 | ripgrep | `ignore::types::<TypesBuilder>::build` | 645 | 164 | 14 | 156 | 156 | constraints 699, liveness 353, moves 315 |
| 1625 | 2464 | ripgrep | `grep_regex::strip::strip_from_match_ascii` | 827 | 143 | 19 | 132 | 132 | constraints 717, moves 342, liveness 281 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 7795 | 6365 | ripgrep | `ignore::default_types::DEFAULT_TYPES` | 10832 | 10683 | 0 | 10305 | 10305 | constraints 3230, infer 1128, liveness 1076 |
| 3049 | 3938 | ripgrep | `rg::flags::hiargs::<HiArgs>::from_low_args` | 1715 | 365 | 52 | 322 | 322 | constraints 1155, liveness 690, moves 645 |
| 2751 | 5494 | ripgrep | `ignore::dir::<Ignore>::matched_ignore` | 1130 | 548 | 24 | 662 | 662 | constraints 1433, liveness 503, moves 380 |
| 2727 | 4643 | ripgrep | `rg::flags::lowargs::<LowArgs>::fmt` | 2498 | 2573 | 234 | 3813 | 3813 | constraints 1006, liveness 694, moves 368 |
| 2704 | 6058 | ripgrep | `rg::search_parallel` | 1342 | 488 | 58 | 464 | 464 | constraints 952, liveness 633, moves 550 |
| 2501 | 4321 | ripgrep | `rg::flags::hiargs::<HiArgs>::fmt` | 2274 | 2342 | 213 | 3470 | 3470 | constraints 962, liveness 604, moves 334 |
| 2465 | 5536 | ripgrep | `ignore::dir::<Ignore>::add_child_path_with_found_ignore_files` | 911 | 465 | 19 | 463 | 461 | constraints 1411, liveness 375, moves 333 |
| 2398 | 5798 | ripgrep | `ignore::walk::<WalkParallel>::visit` | 817 | 340 | 30 | 401 | 397 | constraints 1147, liveness 500, moves 423 |
| 2353 | 4602 | ripgrep | `rg::flags::doc::help::generate_long_flag` | 1286 | 815 | 57 | 812 | 812 | constraints 1133, moves 406, liveness 378 |
| 2159 | 3512 | ripgrep | `globset::<GlobSet>::new` | 1164 | 365 | 54 | 309 | 309 | constraints 786, liveness 512, moves 446 |
| 2053 | 2157 | ripgrep | `rg::search` | 886 | 212 | 27 | 190 | 190 | constraints 729, liveness 524, moves 373 |
| 1982 | 4228 | ripgrep | `grep_printer::standard::<StandardImpl<'a, M, W>>::write_exceeded_line` | 1022 | 367 | 14 | 381 | 381 | constraints 1038, moves 338, liveness 282 |
| 1732 | 2531 | ripgrep | `grep_regex::config::<ConfiguredHIR>::new` | 988 | 310 | 40 | 282 | 282 | constraints 620, liveness 435, moves 385 |
| 1638 | 3220 | ripgrep | `rg::flags::parse::<Parser>::parse` | 908 | 311 | 22 | 287 | 287 | constraints 615, moves 409, liveness 330 |
| 1631 | 3447 | ripgrep | `grep_regex::ban::check` | 818 | 290 | 18 | 289 | 281 | constraints 839, moves 295, liveness 240 |
| 1539 | 1848 | ripgrep | `rg::flags::hiargs::<HiArgs>::walk_builder` | 801 | 263 | 35 | 241 | 241 | constraints 587, liveness 397, moves 278 |
| 1530 | 2533 | ripgrep | `ignore::types::<TypesBuilder>::build` | 645 | 164 | 14 | 156 | 156 | constraints 659, liveness 362, moves 284 |
| 1499 | 3964 | ripgrep | `rg::flags::doc::man::generate_flag` | 1052 | 706 | 49 | 683 | 683 | constraints 662, moves 318, liveness 222 |
| 1468 | 2464 | ripgrep | `grep_regex::strip::strip_from_match_ascii` | 827 | 143 | 19 | 132 | 132 | constraints 650, moves 299, liveness 275 |
| 1454 | 2546 | ripgrep | `ignore::incremental::<IncrementalIgnore>::matched_with_errors_ignore` | 730 | 222 | 29 | 243 | 243 | constraints 594, liveness 359, moves 223 |
