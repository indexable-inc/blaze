| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| ripgrep | polonius | 2980 | 2247 | 610 | 325 | 0 | 53.4% | 878 | 33 | 1137 | 7194 |
| ripgrep | nll | 2980 | 2247 | 610 | 281 | 0 | 46.1% | 878 | 29 | 1022 | 6729 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 127 | 7 | 2 | 9 | 64 | 6 | 0 | 43 | 11 | 1 | 29 |
| nll | 111 | 6 | 1 | 9 | 59 | 5 | 0 | 38 | 1 | 0 | 27 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 7194 | 6284 | ripgrep | `ignore::default_types::DEFAULT_TYPES` | 10832 | 10683 | 0 | 10305 | 10305 | constraints 2630, liveness 1001, infer 892 |
| 2943 | 4463 | ripgrep | `rg::flags::lowargs::<LowArgs>::fmt` | 2498 | 2573 | 234 | 3813 | 3813 | constraints 1003, liveness 728, moves 288 |
| 2675 | 4117 | ripgrep | `rg::flags::hiargs::<HiArgs>::fmt` | 2274 | 2342 | 213 | 3470 | 3470 | constraints 898, liveness 652, moves 279 |
| 2538 | 4700 | ripgrep | `ignore::dir::<Ignore>::matched_ignore` | 1130 | 548 | 24 | 662 | 662 | constraints 1351, liveness 466, moves 270 |
| 2392 | 3497 | ripgrep | `rg::flags::hiargs::<HiArgs>::from_low_args` | 1715 | 365 | 52 | 322 | 322 | constraints 754, liveness 691, moves 359 |
| 2233 | 4292 | ripgrep | `rg::flags::doc::help::generate_long_flag` | 1286 | 815 | 57 | 812 | 812 | constraints 1116, liveness 352, moves 230 |
| 2219 | 4894 | ripgrep | `rg::search_parallel` | 1342 | 488 | 58 | 464 | 464 | constraints 702, liveness 578, walk 318 |
| 2136 | 5107 | ripgrep | `ignore::dir::<Ignore>::add_child_path_with_found_ignore_files` | 911 | 465 | 19 | 463 | 461 | constraints 1222, liveness 345, moves 180 |
| 1819 | 3318 | ripgrep | `globset::<GlobSet>::new` | 1164 | 365 | 54 | 309 | 309 | constraints 527, liveness 507, moves 272 |
| 1792 | 4959 | ripgrep | `ignore::walk::<WalkParallel>::visit` | 817 | 340 | 30 | 401 | 397 | constraints 760, liveness 413, moves 233 |
| 1688 | 3972 | ripgrep | `grep_printer::standard::<StandardImpl<'a, M, W>>::write_exceeded_line` | 1022 | 367 | 14 | 381 | 381 | constraints 861, liveness 316, moves 200 |
| 1596 | 1934 | ripgrep | `rg::search` | 886 | 212 | 27 | 190 | 190 | liveness 479, constraints 421, moves 239 |
| 1556 | 3163 | ripgrep | `grep_regex::ban::check` | 818 | 290 | 18 | 289 | 281 | constraints 814, liveness 245, moves 162 |
| 1435 | 2493 | ripgrep | `rg::flags::defs::FLAGS` | 1523 | 879 | 0 | 879 | 879 | liveness 472, constraints 344, loans 202 |
| 1431 | 2395 | ripgrep | `grep_regex::config::<ConfiguredHIR>::new` | 988 | 310 | 40 | 282 | 282 | liveness 445, constraints 435, moves 225 |
| 1410 | 3629 | ripgrep | `rg::flags::doc::man::generate_flag` | 1052 | 706 | 49 | 683 | 683 | constraints 659, liveness 224, moves 187 |
| 1405 | 2273 | ripgrep | `grep_regex::strip::strip_from_match_ascii` | 827 | 143 | 19 | 132 | 132 | constraints 637, liveness 303, moves 185 |
| 1395 | 1873 | ripgrep | `rg::flags::doc::help::remove_roff` | 672 | 415 | 27 | 339 | 339 | constraints 652, liveness 229, walk 171 |
| 1378 | 3054 | ripgrep | `rg::flags::parse::<Parser>::parse` | 908 | 311 | 22 | 287 | 287 | constraints 482, liveness 340, moves 207 |
| 1301 | 2123 | ripgrep | `ignore::gitignore::<GitignoreBuilder>::add_line` | 897 | 327 | 24 | 326 | 326 | constraints 571, liveness 254, moves 158 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 6729 | 6284 | ripgrep | `ignore::default_types::DEFAULT_TYPES` | 10832 | 10683 | 0 | 10305 | 10305 | constraints 2468, infer 1058, liveness 996 |
| 2618 | 4463 | ripgrep | `rg::flags::lowargs::<LowArgs>::fmt` | 2498 | 2573 | 234 | 3813 | 3813 | constraints 940, liveness 739, moves 281 |
| 2376 | 4700 | ripgrep | `ignore::dir::<Ignore>::matched_ignore` | 1130 | 548 | 24 | 662 | 662 | constraints 1270, liveness 461, moves 252 |
| 2285 | 4117 | ripgrep | `rg::flags::hiargs::<HiArgs>::fmt` | 2274 | 2342 | 213 | 3470 | 3470 | constraints 840, liveness 616, moves 254 |
| 2283 | 3497 | ripgrep | `rg::flags::hiargs::<HiArgs>::from_low_args` | 1715 | 365 | 52 | 322 | 322 | constraints 745, liveness 658, moves 352 |
| 2150 | 4292 | ripgrep | `rg::flags::doc::help::generate_long_flag` | 1286 | 815 | 57 | 812 | 812 | constraints 1031, liveness 416, moves 253 |
| 2080 | 4894 | ripgrep | `rg::search_parallel` | 1342 | 488 | 58 | 464 | 464 | constraints 678, liveness 573, walk 310 |
| 2023 | 5107 | ripgrep | `ignore::dir::<Ignore>::add_child_path_with_found_ignore_files` | 911 | 465 | 19 | 463 | 461 | constraints 1165, liveness 357, moves 171 |
| 1666 | 3318 | ripgrep | `globset::<GlobSet>::new` | 1164 | 365 | 54 | 309 | 309 | liveness 483, constraints 478, moves 263 |
| 1632 | 4959 | ripgrep | `ignore::walk::<WalkParallel>::visit` | 817 | 340 | 30 | 401 | 397 | constraints 687, liveness 406, moves 211 |
| 1533 | 3972 | ripgrep | `grep_printer::standard::<StandardImpl<'a, M, W>>::write_exceeded_line` | 1022 | 367 | 14 | 381 | 381 | constraints 796, liveness 275, moves 183 |
| 1465 | 1934 | ripgrep | `rg::search` | 886 | 212 | 27 | 190 | 190 | liveness 456, constraints 420, walk 214 |
| 1367 | 3163 | ripgrep | `grep_regex::ban::check` | 818 | 290 | 18 | 289 | 281 | constraints 746, liveness 226, moves 153 |
| 1302 | 2493 | ripgrep | `rg::flags::defs::FLAGS` | 1523 | 879 | 0 | 879 | 879 | liveness 430, constraints 302, loans 198 |
| 1286 | 2273 | ripgrep | `grep_regex::strip::strip_from_match_ascii` | 827 | 143 | 19 | 132 | 132 | constraints 613, liveness 262, moves 175 |
| 1276 | 2395 | ripgrep | `grep_regex::config::<ConfiguredHIR>::new` | 988 | 310 | 40 | 282 | 282 | liveness 406, constraints 387, moves 207 |
| 1274 | 3629 | ripgrep | `rg::flags::doc::man::generate_flag` | 1052 | 706 | 49 | 683 | 683 | constraints 614, liveness 211, moves 175 |
| 1242 | 1873 | ripgrep | `rg::flags::doc::help::remove_roff` | 672 | 415 | 27 | 339 | 339 | constraints 597, liveness 230, walk 160 |
| 1208 | 3054 | ripgrep | `rg::flags::parse::<Parser>::parse` | 908 | 311 | 22 | 287 | 287 | constraints 426, liveness 319, moves 193 |
| 1197 | 2123 | ripgrep | `ignore::gitignore::<GitignoreBuilder>::add_line` | 897 | 327 | 24 | 326 | 326 | constraints 529, liveness 244, moves 160 |
