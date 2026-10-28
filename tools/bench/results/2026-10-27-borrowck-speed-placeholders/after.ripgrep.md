| App | Mode | Bodies | Infer ms (cold) | Infer ms (edit) | Borrowck ms (edit, excl. MIR) | MIR ms | Borrowck / infer (edit) | First check ms (cold) | p50 us | p99 us | max us |
|---|---|---|---|---|---|---|---|---|---|---|---|
| ripgrep | polonius | 2980 | 2119 | 580 | 318 | 0 | 54.8% | 832 | 32 | 1110 | 7084 |
| ripgrep | nll | 2980 | 2119 | 580 | 276 | 0 | 47.6% | 832 | 28 | 989 | 7183 |

Where the time goes (ms, summed over the apps):

| Mode | constraints | entry_sets | errors | infer | liveness | loans | mir | moves | scopes | setup | walk |
|---|---|---|---|---|---|---|---|---|---|---|---|
| polonius | 121 | 7 | 2 | 9 | 64 | 6 | 0 | 43 | 11 | 0 | 29 |
| nll | 107 | 6 | 1 | 9 | 59 | 5 | 0 | 39 | 1 | 0 | 26 |

Slowest bodies, polonius:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 7084 | 5884 | ripgrep | `ignore::default_types::DEFAULT_TYPES` | 10832 | 10683 | 0 | 10305 | 10305 | constraints 2452, liveness 1112, infer 936 |
| 2800 | 4216 | ripgrep | `rg::flags::lowargs::<LowArgs>::fmt` | 2498 | 2573 | 234 | 3813 | 3813 | constraints 908, liveness 696, moves 284 |
| 2616 | 3918 | ripgrep | `rg::flags::hiargs::<HiArgs>::fmt` | 2274 | 2342 | 213 | 3470 | 3470 | constraints 894, liveness 618, moves 269 |
| 2457 | 4475 | ripgrep | `ignore::dir::<Ignore>::matched_ignore` | 1130 | 548 | 24 | 662 | 662 | constraints 1282, liveness 475, moves 266 |
| 2369 | 3357 | ripgrep | `rg::flags::hiargs::<HiArgs>::from_low_args` | 1715 | 365 | 52 | 322 | 322 | constraints 743, liveness 671, moves 363 |
| 2195 | 4083 | ripgrep | `rg::flags::doc::help::generate_long_flag` | 1286 | 815 | 57 | 812 | 812 | constraints 1019, liveness 400, moves 240 |
| 2181 | 4792 | ripgrep | `rg::search_parallel` | 1342 | 488 | 58 | 464 | 464 | constraints 686, liveness 579, walk 315 |
| 2079 | 4816 | ripgrep | `ignore::dir::<Ignore>::add_child_path_with_found_ignore_files` | 911 | 465 | 19 | 463 | 461 | constraints 1183, liveness 344, moves 187 |
| 1753 | 3193 | ripgrep | `globset::<GlobSet>::new` | 1164 | 365 | 54 | 309 | 309 | constraints 503, liveness 502, moves 260 |
| 1715 | 4752 | ripgrep | `ignore::walk::<WalkParallel>::visit` | 817 | 340 | 30 | 401 | 397 | constraints 692, liveness 401, moves 232 |
| 1576 | 1876 | ripgrep | `rg::search` | 887 | 212 | 27 | 190 | 190 | liveness 474, constraints 409, moves 249 |
| 1559 | 3741 | ripgrep | `grep_printer::standard::<StandardImpl<'a, M, W>>::write_exceeded_line` | 1022 | 367 | 14 | 381 | 381 | constraints 777, liveness 277, moves 188 |
| 1482 | 2289 | ripgrep | `grep_regex::config::<ConfiguredHIR>::new` | 988 | 310 | 40 | 282 | 282 | liveness 515, constraints 408, moves 220 |
| 1453 | 3010 | ripgrep | `grep_regex::ban::check` | 818 | 290 | 18 | 289 | 281 | constraints 778, liveness 236, moves 158 |
| 1416 | 2405 | ripgrep | `rg::flags::defs::FLAGS` | 1523 | 879 | 0 | 879 | 879 | liveness 475, constraints 331, loans 195 |
| 1387 | 3407 | ripgrep | `rg::flags::doc::man::generate_flag` | 1052 | 706 | 49 | 683 | 683 | constraints 636, liveness 230, moves 172 |
| 1377 | 2895 | ripgrep | `rg::flags::parse::<Parser>::parse` | 912 | 311 | 22 | 287 | 287 | constraints 521, liveness 324, moves 211 |
| 1362 | 1793 | ripgrep | `rg::flags::doc::help::remove_roff` | 672 | 415 | 27 | 339 | 339 | constraints 644, liveness 231, walk 181 |
| 1337 | 2196 | ripgrep | `grep_regex::strip::strip_from_match_ascii` | 827 | 143 | 19 | 132 | 132 | constraints 609, liveness 272, moves 190 |
| 1276 | 1291 | ripgrep | `globset::glob::<Parser<'a>>::parse_star` | 1142 | 156 | 30 | 200 | 200 | constraints 405, liveness 330, moves 214 |

Slowest bodies, nll:

| us | infer us | App | Body | points | regions | loans | constraints | localized | top phases |
|---|---|---|---|---|---|---|---|---|---|
| 7183 | 5884 | ripgrep | `ignore::default_types::DEFAULT_TYPES` | 10832 | 10683 | 0 | 10305 | 10305 | constraints 2420, liveness 1286, infer 1286 |
| 2531 | 4216 | ripgrep | `rg::flags::lowargs::<LowArgs>::fmt` | 2498 | 2573 | 234 | 3813 | 3813 | constraints 910, liveness 690, moves 291 |
| 2360 | 4475 | ripgrep | `ignore::dir::<Ignore>::matched_ignore` | 1130 | 548 | 24 | 662 | 662 | constraints 1224, liveness 456, moves 258 |
| 2279 | 3357 | ripgrep | `rg::flags::hiargs::<HiArgs>::from_low_args` | 1715 | 365 | 52 | 322 | 322 | constraints 715, liveness 689, moves 350 |
| 2255 | 3918 | ripgrep | `rg::flags::hiargs::<HiArgs>::fmt` | 2274 | 2342 | 213 | 3470 | 3470 | constraints 799, liveness 623, moves 257 |
| 2033 | 4792 | ripgrep | `rg::search_parallel` | 1342 | 488 | 58 | 464 | 464 | constraints 656, liveness 551, walk 304 |
| 1993 | 4083 | ripgrep | `rg::flags::doc::help::generate_long_flag` | 1286 | 815 | 57 | 812 | 812 | constraints 985, liveness 365, moves 220 |
| 1976 | 4816 | ripgrep | `ignore::dir::<Ignore>::add_child_path_with_found_ignore_files` | 911 | 465 | 19 | 463 | 461 | constraints 1147, liveness 327, moves 182 |
| 1645 | 3193 | ripgrep | `globset::<GlobSet>::new` | 1164 | 365 | 54 | 309 | 309 | liveness 494, constraints 470, moves 257 |
| 1581 | 4752 | ripgrep | `ignore::walk::<WalkParallel>::visit` | 817 | 340 | 30 | 401 | 397 | constraints 664, liveness 385, moves 211 |
| 1478 | 3741 | ripgrep | `grep_printer::standard::<StandardImpl<'a, M, W>>::write_exceeded_line` | 1022 | 367 | 14 | 381 | 381 | constraints 754, liveness 272, moves 184 |
| 1394 | 1876 | ripgrep | `rg::search` | 887 | 212 | 27 | 190 | 190 | liveness 455, constraints 361, moves 213 |
| 1341 | 3010 | ripgrep | `grep_regex::ban::check` | 818 | 290 | 18 | 289 | 281 | constraints 711, liveness 218, moves 165 |
| 1303 | 2405 | ripgrep | `rg::flags::defs::FLAGS` | 1523 | 879 | 0 | 879 | 879 | liveness 437, constraints 294, loans 207 |
| 1301 | 2196 | ripgrep | `grep_regex::strip::strip_from_match_ascii` | 827 | 143 | 19 | 132 | 132 | constraints 590, liveness 265, moves 213 |
| 1282 | 2289 | ripgrep | `grep_regex::config::<ConfiguredHIR>::new` | 988 | 310 | 40 | 282 | 282 | liveness 405, constraints 390, moves 214 |
| 1242 | 3407 | ripgrep | `rg::flags::doc::man::generate_flag` | 1052 | 706 | 49 | 683 | 683 | constraints 598, liveness 213, moves 161 |
| 1239 | 1793 | ripgrep | `rg::flags::doc::help::remove_roff` | 672 | 415 | 27 | 339 | 339 | constraints 596, liveness 222, walk 162 |
| 1187 | 2895 | ripgrep | `rg::flags::parse::<Parser>::parse` | 912 | 311 | 22 | 287 | 287 | constraints 424, liveness 304, moves 196 |
| 1171 | 1291 | ripgrep | `globset::glob::<Parser<'a>>::parse_star` | 1142 | 156 | 30 | 200 | 200 | constraints 382, liveness 298, moves 193 |
